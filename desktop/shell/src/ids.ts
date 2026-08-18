/// Identifiers this client mints for the runtime.
///
/// Version 7, not 4, and that is a correctness choice rather than a fashion:
/// `list_session_heads` orders by `(session_id, branch_id)` ascending, so the
/// order the runtime returns conversations in *is* the order their ids sort
/// in. Mint random v4 ids and the list is arbitrary — and any "最近" the screen
/// printed over it would be invented. A v7 id carries its own creation time in
/// the high bits, so sorting by id is sorting by when the conversation started.
///
/// The runtime mints v7 internally (`Uuid::now_v7()`); this is the same
/// decision made on the same grounds, not a mirror of a constant.

/// Guards against two ids minted inside the same millisecond sorting
/// arbitrarily against each other, which would put two conversations started in
/// the same tick in the wrong order.
let lastMillis = -1;
let lastCounter = 0;

export function uuidv7(now: () => number = Date.now): string {
  const millis = now();
  if (millis === lastMillis) {
    lastCounter += 1;
  } else {
    lastMillis = millis;
    lastCounter = 0;
  }
  // 12 bits of counter is what rand_a holds; past that, fall back to letting
  // the random tail decide rather than silently wrapping into the next
  // millisecond's range.
  const counter = lastCounter & 0xfff;

  const bytes = new Uint8Array(16);
  // 48-bit big-endian milliseconds.
  const ms = BigInt(millis);
  for (let i = 0; i < 6; i += 1) {
    bytes[i] = Number((ms >> BigInt(8 * (5 - i))) & 0xffn);
  }
  bytes[6] = 0x70 | ((counter >> 8) & 0x0f); // version 7 + counter high nibble
  bytes[7] = counter & 0xff;

  const tail = new Uint8Array(8);
  crypto.getRandomValues(tail);
  bytes.set(tail, 8);
  bytes[8] = 0x80 | (bytes[8] & 0x3f); // variant 10

  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
