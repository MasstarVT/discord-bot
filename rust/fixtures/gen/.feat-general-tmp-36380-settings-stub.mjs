
export const getSettingsSafe = async () => globalThis.__settings ?? null;
export const peekSettings = () => globalThis.__settings ?? null;
export const updateSettings = async (g, data) => { (globalThis.__updates ??= []).push(data); return { ...(globalThis.__settings ?? {}), ...data }; };
export const refreshSnapshot = async () => ({ ok: true });
export const cache = { invalidate: async () => 0 };
