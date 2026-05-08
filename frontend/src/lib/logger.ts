function prefix(): string {
  return `[astrcode ${new Date().toISOString().slice(11, 23)}]`;
}

export const log = {
  info(...args: unknown[]) {
    console.log(prefix(), ...args);
  },
  warn(...args: unknown[]) {
    console.warn(prefix(), ...args);
  },
  error(...args: unknown[]) {
    console.error(prefix(), ...args);
  },
};
