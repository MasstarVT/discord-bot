//! A syntax checker that follows V8's regexp parser (regexp-parser.cc) closely enough to accept and refuse the same
//! patterns with the same error text, for the flags users may set (i, m, s, u). `new RegExp(p, f)` in the JS is the
//! validity test of safeRegex, and its error message is shown to the admin, so both are reproduced here. Matching
//! itself is done by regress.

const INFINITY: u64 = i32::MAX as u64;

struct Parser<'a> {
    p: &'a [u16],
    i: usize,
    unicode: bool,
    /// Captures opened so far.
    captures_started: usize,
    /// Total captures in the pattern (scanned lazily).
    total_captures: Option<usize>,
    has_named_captures: Option<bool>,
    /// Named captures with the alternative path they sit in.
    names: Vec<(Vec<u16>, Vec<(usize, usize)>)>,
    named_refs: Vec<Vec<u16>>,
    /// Disjunction stack: (disjunction id, alternative index).
    path: Vec<(usize, usize)>,
    next_disjunction: usize,
}

const END: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Capture,
    Plain,
    Lookahead,
    Lookbehind,
}

/// Why V8 refuses a pattern (the text after "Invalid regular expression: /p/f: ").
pub type V8Error = &'static str;

impl<'a> Parser<'a> {
    fn cur(&self) -> u32 {
        self.at(self.i)
    }

    fn at(&self, i: usize) -> u32 {
        match self.p.get(i) {
            None => END,
            Some(&c) => {
                if self.unicode && (0xd800..0xdc00).contains(&c)
                    && let Some(&d) = self.p.get(i + 1)
                        && (0xdc00..0xe000).contains(&d) {
                            return 0x10000 + (((c as u32) - 0xd800) << 10) + ((d as u32) - 0xdc00);
                        }
                c as u32
            }
        }
    }

    fn width(&self, i: usize) -> usize {
        if self.at(i) > 0xffff && self.at(i) != END { 2 } else { 1 }
    }

    fn next(&self) -> u32 {
        if self.i >= self.p.len() {
            return END;
        }
        self.at(self.i + self.width(self.i))
    }

    fn advance(&mut self) {
        if self.i < self.p.len() {
            self.i += self.width(self.i);
        } else {
            self.i = self.p.len() + 1;
        }
    }

    fn advance_n(&mut self, n: usize) {
        for _ in 0..n {
            self.advance();
        }
    }

    fn has_more(&self) -> bool {
        self.i < self.p.len()
    }

    fn is_digit(c: u32) -> bool {
        (b'0' as u32..=b'9' as u32).contains(&c)
    }

    fn ch(c: u8) -> u32 {
        c as u32
    }

    // Scans the whole pattern once for capture groups (V8's ScanForCaptures).
    fn scan_captures(&mut self) {
        if self.total_captures.is_some() {
            return;
        }
        let mut n = 0;
        let mut named = false;
        let mut i = 0;
        let p = self.p;
        let mut in_class = false;
        while i < p.len() {
            let c = p[i];
            if c == b'\\' as u16 {
                i += 2;
                continue;
            }
            if in_class {
                if c == b']' as u16 {
                    in_class = false;
                }
            } else if c == b'[' as u16 {
                in_class = true;
            } else if c == b'(' as u16 {
                if p.get(i + 1) == Some(&(b'?' as u16)) {
                    if p.get(i + 2) == Some(&(b'<' as u16)) && p.get(i + 3) != Some(&(b'=' as u16)) && p.get(i + 3) != Some(&(b'!' as u16)) {
                        n += 1;
                        named = true;
                    }
                } else {
                    n += 1;
                }
            }
            i += 1;
        }
        self.total_captures = Some(n);
        self.has_named_captures = Some(named);
    }

    fn has_named(&mut self) -> bool {
        self.scan_captures();
        self.has_named_captures.unwrap_or(false)
    }

    fn parse_interval(&mut self) -> Option<(u64, u64)> {
        let start = self.i;
        self.advance(); // '{'
        if !Self::is_digit(self.cur()) {
            self.i = start;
            return None;
        }
        let mut min: u64 = 0;
        while Self::is_digit(self.cur()) {
            min = (min * 10 + (self.cur() - b'0' as u32) as u64).min(INFINITY);
            self.advance();
        }
        let max;
        if self.cur() == Self::ch(b'}') {
            max = min;
            self.advance();
        } else if self.cur() == Self::ch(b',') {
            self.advance();
            if self.cur() == Self::ch(b'}') {
                max = INFINITY;
                self.advance();
            } else {
                let mut m: u64 = 0;
                while Self::is_digit(self.cur()) {
                    m = (m * 10 + (self.cur() - b'0' as u32) as u64).min(INFINITY);
                    self.advance();
                }
                if self.cur() != Self::ch(b'}') {
                    self.i = start;
                    return None;
                }
                max = m;
                self.advance();
            }
        } else {
            self.i = start;
            return None;
        }
        Some((min, max))
    }

    fn hex(c: u32) -> Option<u32> {
        char::from_u32(c).and_then(|ch| ch.to_digit(16))
    }

    fn parse_hex(&mut self, n: usize) -> Option<u32> {
        let start = self.i;
        let mut v = 0;
        for _ in 0..n {
            match Self::hex(self.cur()) {
                Some(d) => {
                    v = v * 16 + d;
                    self.advance();
                }
                None => {
                    self.i = start;
                    return None;
                }
            }
        }
        Some(v)
    }

    // After '\u' has been consumed.
    fn parse_unicode_escape(&mut self) -> Option<u32> {
        if self.cur() == Self::ch(b'{') && self.unicode {
            let start = self.i;
            self.advance();
            let mut v: u64 = 0;
            let mut any = false;
            while let Some(d) = Self::hex(self.cur()) {
                v = v * 16 + d as u64;
                any = true;
                if v > 0x10ffff {
                    self.i = start;
                    return None;
                }
                self.advance();
            }
            if !any || self.cur() != Self::ch(b'}') {
                self.i = start;
                return None;
            }
            self.advance();
            return Some(v as u32);
        }
        let v = self.parse_hex(4)?;
        if self.unicode && (0xd800..0xdc00).contains(&v) && self.cur() == Self::ch(b'\\') && self.next() == Self::ch(b'u') {
            let save = self.i;
            self.advance_n(2);
            if let Some(t) = self.parse_hex(4)
                && (0xdc00..0xe000).contains(&t) {
                    return Some(0x10000 + ((v - 0xd800) << 10) + (t - 0xdc00));
                }
            self.i = save;
        }
        Some(v)
    }

    fn parse_octal(&mut self) -> u32 {
        // Up to 3 digits, value ≤ 0o377.
        let mut v = 0;
        let mut n = 0;
        while n < 3 && (Self::ch(b'0')..=Self::ch(b'7')).contains(&self.cur()) {
            let nv = v * 8 + (self.cur() - b'0' as u32);
            if nv > 0o377 {
                break;
            }
            v = nv;
            self.advance();
            n += 1;
        }
        v
    }

    fn is_syntax_or_slash(c: u32) -> bool {
        matches!(char::from_u32(c), Some('^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '/'))
    }

    // At the char after '\' (current() is that char). Returns the escaped value.
    fn parse_character_escape(&mut self, in_class: bool) -> Result<u32, V8Error> {
        let c = self.cur();
        match char::from_u32(c) {
            Some('f') => {
                self.advance();
                return Ok(0xc);
            }
            Some('n') => {
                self.advance();
                return Ok(0xa);
            }
            Some('r') => {
                self.advance();
                return Ok(0xd);
            }
            Some('t') => {
                self.advance();
                return Ok(0x9);
            }
            Some('v') => {
                self.advance();
                return Ok(0xb);
            }
            Some('c') => {
                let control = self.next();
                let letter = control & !(b'A' as u32 ^ b'a' as u32);
                if (Self::ch(b'A')..=Self::ch(b'Z')).contains(&letter) {
                    self.advance_n(2);
                    return Ok(control & 0x1f);
                }
                if self.unicode {
                    return Err("Invalid Unicode escape");
                }
                if in_class && (Self::is_digit(control) || control == Self::ch(b'_')) {
                    self.advance_n(2);
                    return Ok(control & 0x1f);
                }
                // An identity escape of the backslash: '\' then 'c' is read as a literal.
                return Ok(b'\\' as u32);
            }
            Some('0') if self.unicode && !Self::is_digit(self.next()) => {
                self.advance();
                return Ok(0);
            }
            Some('0'..='7') => {
                if self.unicode {
                    return Err("Invalid class escape");
                }
                return Ok(self.parse_octal());
            }
            Some('x') => {
                self.advance();
                if let Some(v) = self.parse_hex(2) {
                    return Ok(v);
                }
                if self.unicode {
                    return Err("Invalid escape");
                }
                return Ok(b'x' as u32);
            }
            Some('u') => {
                self.advance();
                if let Some(v) = self.parse_unicode_escape() {
                    return Ok(v);
                }
                if self.unicode {
                    return Err("Invalid Unicode escape");
                }
                return Ok(b'u' as u32);
            }
            _ => {}
        }
        if !self.unicode || Self::is_syntax_or_slash(c) || (in_class && c == Self::ch(b'-')) {
            self.advance();
            return Ok(c);
        }
        Err("Invalid escape")
    }

    fn parse_property(&mut self, in_class: bool) -> Result<(), V8Error> {
        // current() is '{' after '\p'.
        let err = if in_class { "Invalid property name in character class" } else { "Invalid property name" };
        if self.cur() != Self::ch(b'{') {
            return Err(err);
        }
        let start = self.i;
        while self.has_more() && self.cur() != Self::ch(b'}') {
            self.advance();
        }
        if !self.has_more() {
            return Err(err);
        }
        let name = String::from_utf16_lossy(&self.p[start + 1..self.i]);
        self.advance();
        let ok = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '=')
            && regress::Regex::with_flags(&format!("\\p{{{name}}}"), "u").is_ok();
        if ok { Ok(()) } else { Err(err) }
    }

    // ClassEscape inside [...]; returns (char, is_class).
    fn parse_class_atom(&mut self) -> Result<(u32, bool), V8Error> {
        if self.cur() != Self::ch(b'\\') {
            let c = self.cur();
            self.advance();
            return Ok((c, false));
        }
        let next = self.next();
        match char::from_u32(next) {
            Some('b') => {
                self.advance_n(2);
                return Ok((8, false));
            }
            Some('-') if self.unicode => {
                self.advance_n(2);
                return Ok((next, false));
            }
            _ if next == END => return Err("\\ at end of pattern"),
            Some('d' | 'D' | 's' | 'S' | 'w' | 'W') => {
                self.advance_n(2);
                return Ok((0, true));
            }
            Some('p' | 'P') if self.unicode => {
                self.advance_n(2);
                self.parse_property(true)?;
                return Ok((0, true));
            }
            _ => {}
        }
        self.advance(); // '\'
        Ok((self.parse_character_escape(true)?, false))
    }

    fn parse_class(&mut self) -> Result<(), V8Error> {
        self.advance(); // '['
        if self.cur() == Self::ch(b'^') {
            self.advance();
        }
        while self.has_more() && self.cur() != Self::ch(b']') {
            let (c1, class1) = self.parse_class_atom()?;
            if self.cur() == Self::ch(b'-') {
                self.advance();
                if !self.has_more() {
                    break;
                }
                if self.cur() == Self::ch(b']') {
                    break;
                }
                let (c2, class2) = self.parse_class_atom()?;
                if class1 || class2 {
                    if self.unicode {
                        return Err("Invalid character class");
                    }
                    continue;
                }
                if c1 > c2 {
                    return Err("Range out of order in character class");
                }
            }
        }
        if !self.has_more() {
            return Err("Unterminated character class");
        }
        self.advance();
        Ok(())
    }

    fn is_id_start(c: u32) -> bool {
        c == Self::ch(b'$') || c == Self::ch(b'_') || char::from_u32(c).is_some_and(unicode_ident::is_xid_start)
    }

    fn is_id_part(c: u32) -> bool {
        c == Self::ch(b'$') || c == 0x200c || c == 0x200d || char::from_u32(c).is_some_and(unicode_ident::is_xid_continue)
    }

    // Reads a group name up to '>' (current() is the first name char). Surrogate pairs combine even without u.
    fn parse_group_name(&mut self) -> Result<Vec<u16>, V8Error> {
        let saved_unicode = self.unicode;
        self.unicode = true; // names read whole code points
        let mut name: Vec<u16> = Vec::new();
        let mut at_start = true;
        let r = loop {
            let mut c = self.cur();
            self.advance();
            if c == Self::ch(b'\\') && self.cur() == Self::ch(b'u') {
                self.advance();
                match self.parse_unicode_escape() {
                    Some(v) => c = v,
                    None => break Err("Invalid Unicode escape"),
                }
            }
            if c == Self::ch(b'\\') || c == END {
                break Err("Invalid capture group name");
            }
            if at_start {
                if !Self::is_id_start(c) {
                    break Err("Invalid capture group name");
                }
            } else if c == Self::ch(b'>') {
                break Ok(name.clone());
            } else if !Self::is_id_part(c) {
                break Err("Invalid capture group name");
            }
            if let Some(ch) = char::from_u32(c) {
                let mut buf = [0u16; 2];
                name.extend_from_slice(ch.encode_utf16(&mut buf));
            } else {
                name.push(c as u16);
            }
            at_start = false;
        };
        self.unicode = saved_unicode;
        r
    }

    fn parse_modifiers(&mut self) -> Result<(), V8Error> {
        // current() is the first of [-ims].
        let mut seen: Vec<u32> = Vec::new();
        let mut dash = false;
        let mut after_dash = 0;
        let mut before_dash = 0;
        loop {
            let c = self.cur();
            match char::from_u32(c) {
                Some('i' | 'm' | 's') => {
                    if seen.contains(&c) {
                        return Err("Repeated flag in flag group");
                    }
                    seen.push(c);
                    if dash {
                        after_dash += 1;
                    } else {
                        before_dash += 1;
                    }
                }
                Some('-') => {
                    if dash {
                        return Err("Multiple dashes in flag group");
                    }
                    dash = true;
                }
                Some(':') => {
                    if dash && after_dash == 0 && before_dash == 0 {
                        return Err("Invalid flag group");
                    }
                    self.advance();
                    return Ok(());
                }
                _ => return Err("Invalid group"),
            }
            self.advance();
        }
    }

    fn check_duplicate_at(&self, name: &[u16], mine: &[(usize, usize)]) -> bool {
        // Two groups may share a name only when they sit in different alternatives of one disjunction.
        self.names.iter().any(|(n, path)| {
            if n.as_slice() != name {
                return false;
            }
            let mut k = 0;
            while k < path.len() && k < mine.len() && path[k] == mine[k] {
                k += 1;
            }
            if k < path.len() && k < mine.len() && path[k].0 == mine[k].0 {
                return false; // same disjunction, different alternative
            }
            true
        })
    }

    fn parse(&mut self) -> Result<(), V8Error> {
        let mut stack: Vec<Group> = Vec::new();
        let mut pending_names: Vec<Option<(Vec<u16>, Vec<(usize, usize)>)>> = Vec::new();
        self.path = vec![(0, 0)];
        self.next_disjunction = 1;
        loop {
            // The last atom added: None for an assertion/start, Some(kind) for something quantifiable.
            let atom: Option<Group>;
            let c = self.cur();
            if c == END {
                if !stack.is_empty() {
                    return Err("Unterminated group");
                }
                for r in &self.named_refs {
                    if !self.names.iter().any(|(n, _)| n == r) {
                        return Err("Invalid named capture referenced");
                    }
                }
                return Ok(());
            }
            match char::from_u32(c) {
                Some(')') => {
                    let Some(g) = stack.pop() else { return Err("Unmatched ')'") };
                    self.path.pop();
                    // V8 registers a group name when the group closes, so later syntax errors inside it win.
                    if let Some(Some((name, path))) = pending_names.pop() {
                        if self.check_duplicate_at(&name, &path) {
                            return Err("Duplicate capture group name");
                        }
                        self.names.push((name, path));
                    }
                    self.advance();
                    atom = Some(g);
                }
                Some('|') => {
                    self.advance();
                    if let Some(last) = self.path.last_mut() {
                        last.1 += 1;
                    }
                    continue;
                }
                Some('*' | '+' | '?') => return Err("Nothing to repeat"),
                Some('^' | '$') => {
                    self.advance();
                    continue;
                }
                Some('.') => {
                    self.advance();
                    atom = Some(Group::Plain);
                }
                Some('(') => {
                    self.advance();
                    let mut kind = Group::Capture;
                    let mut named = false;
                    if self.cur() == Self::ch(b'?') {
                        match char::from_u32(self.next()) {
                            Some(':') => {
                                self.advance_n(2);
                                kind = Group::Plain;
                            }
                            Some('=' | '!') => {
                                self.advance_n(2);
                                kind = Group::Lookahead;
                            }
                            Some('-' | 'i' | 'm' | 's') => {
                                self.advance();
                                self.parse_modifiers()?;
                                kind = Group::Plain;
                            }
                            Some('<') => {
                                self.advance();
                                if self.next() == Self::ch(b'=') || self.next() == Self::ch(b'!') {
                                    self.advance_n(2);
                                    kind = Group::Lookbehind;
                                } else {
                                    named = true;
                                    self.advance();
                                }
                            }
                            _ => return Err("Invalid group"),
                        }
                    }
                    if kind == Group::Capture {
                        self.captures_started += 1;
                        if named {
                            let name = self.parse_group_name()?;
                            pending_names.push(Some((name, self.path.clone())));
                        } else {
                            pending_names.push(None);
                        }
                    } else {
                        pending_names.push(None);
                    }
                    stack.push(kind);
                    self.path.push((self.next_disjunction, 0));
                    self.next_disjunction += 1;
                    continue;
                }
                Some('[') => {
                    self.parse_class()?;
                    atom = Some(Group::Plain);
                }
                Some('\\') => {
                    let n = self.next();
                    if n == END {
                        return Err("\\ at end of pattern");
                    }
                    match char::from_u32(n) {
                        Some('1'..='9') => {
                            // A back reference when the number is at most the number of captures.
                            let save = self.i;
                            self.advance();
                            let mut v: u64 = 0;
                            while Self::is_digit(self.cur()) {
                                v = (v * 10 + (self.cur() - b'0' as u32) as u64).min(INFINITY);
                                self.advance();
                            }
                            let is_backref = if v as usize > self.captures_started {
                                self.scan_captures();
                                v as usize <= self.total_captures.unwrap_or(0)
                            } else {
                                true
                            };
                            if !is_backref {
                                self.i = save;
                                if self.unicode {
                                    return Err("Invalid escape");
                                }
                                if n == Self::ch(b'8') || n == Self::ch(b'9') {
                                    self.advance_n(2);
                                } else {
                                    self.advance();
                                    self.parse_octal();
                                }
                            }
                            atom = Some(Group::Plain);
                        }
                        Some('0') => {
                            self.advance_n(2);
                            if self.unicode && Self::is_digit(self.cur()) {
                                return Err("Invalid decimal escape");
                            }
                            if !self.unicode {
                                // \0 then up to two more octal digits.
                                let mut k = 0;
                                let mut v = 0u32;
                                while k < 2 && (Self::ch(b'0')..=Self::ch(b'7')).contains(&self.cur()) {
                                    let nv = v * 8 + (self.cur() - b'0' as u32);
                                    if nv > 0o377 {
                                        break;
                                    }
                                    v = nv;
                                    self.advance();
                                    k += 1;
                                }
                            }
                            atom = Some(Group::Plain);
                        }
                        Some('b' | 'B') => {
                            self.advance_n(2);
                            continue;
                        }
                        Some('d' | 'D' | 's' | 'S' | 'w' | 'W') => {
                            self.advance_n(2);
                            atom = Some(Group::Plain);
                        }
                        Some('p' | 'P') => {
                            self.advance_n(2);
                            if self.unicode {
                                self.parse_property(false)?;
                            }
                            atom = Some(Group::Plain);
                        }
                        Some('k') if self.unicode || self.has_named() => {
                            self.advance_n(2);
                            if self.cur() != Self::ch(b'<') {
                                return Err("Invalid named reference");
                            }
                            self.advance();
                            let name = self.parse_group_name()?;
                            self.named_refs.push(name);
                            atom = Some(Group::Plain);
                        }
                        _ => {
                            self.advance();
                            self.parse_character_escape(false)?;
                            atom = Some(Group::Plain);
                        }
                    }
                }
                Some('{') => {
                    if self.parse_interval().is_some() {
                        return Err("Nothing to repeat");
                    }
                    if self.unicode {
                        return Err("Lone quantifier brackets");
                    }
                    self.advance();
                    atom = Some(Group::Plain);
                }
                Some('}' | ']') => {
                    if self.unicode {
                        return Err("Lone quantifier brackets");
                    }
                    self.advance();
                    atom = Some(Group::Plain);
                }
                _ => {
                    self.advance();
                    atom = Some(Group::Plain);
                }
            }

            // Quantifier.
            match char::from_u32(self.cur()) {
                Some('*' | '+' | '?') => self.advance(),
                Some('{') => match self.parse_interval() {
                    Some((min, max)) => {
                        if max < min {
                            return Err("numbers out of order in {} quantifier");
                        }
                    }
                    None => {
                        if self.unicode {
                            return Err("Incomplete quantifier");
                        }
                        continue;
                    }
                },
                _ => continue,
            }
            if self.cur() == Self::ch(b'?') {
                self.advance();
            }
            match atom {
                Some(Group::Lookbehind) => return Err("Invalid quantifier"),
                Some(Group::Lookahead) if self.unicode => return Err("Invalid quantifier"),
                _ => {}
            }
        }
    }
}

/// V8's verdict on `new RegExp(pattern, flags)` for flags drawn from i, m, s, u: None when it compiles, else the
/// error text after "Invalid regular expression: /pattern/flags: ".
pub fn check(pattern: &[u16], unicode: bool) -> Option<V8Error> {
    let mut p = Parser {
        p: pattern,
        i: 0,
        unicode,
        captures_started: 0,
        total_captures: None,
        has_named_captures: None,
        names: Vec::new(),
        named_refs: Vec::new(),
        path: Vec::new(),
        next_disjunction: 0,
    };
    p.parse().err()
}
