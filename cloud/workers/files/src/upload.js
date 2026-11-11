// Moves the bytes of an upload from the request body to R2 without passing
// them through wasm. The header is still read and parsed in Rust
// (`readMore`); everything after it stays here. See store.rs.

const ENDED = "the request body ended early";

// Largest read from the request. Reads fill whole blocks, so a large file
// costs a few dozen reads rather than one per network packet.
const BLOCK = 1 << 20;

export class Splitter {
  constructor(stream) {
    this.reader = null;
    this.byob = false;
    if (stream) {
      try {
        this.reader = stream.getReader({ mode: "byob" });
        this.byob = true;
      } catch (e) {
        this.reader = stream.getReader();
      }
    }
    // Chunks read from the request but not yet used.
    this.queue = [];
    // The body ended or failed before a read had what it asked for.
    this.failed = false;
  }

  // Up to `max` bytes from the request (more without a byte reader);
  // undefined at its end. `readAtLeast` is the Workers runtime's own: it
  // waits until the block is full or the body ends.
  async readSource(max) {
    if (!this.reader) return undefined;
    for (;;) {
      let r;
      if (this.byob) {
        const size = Math.min(max, BLOCK);
        r = await this.reader.readAtLeast(size, new Uint8Array(size));
      } else {
        r = await this.reader.read();
      }
      if (r.value && r.value.byteLength > 0) return r.value;
      if (r.done) return undefined;
    }
  }

  // Reads one more chunk from the request, keeps it unused, and returns it
  // for the header parser; undefined at the end of the body.
  async readMore() {
    const c = await this.readSource(64 * 1024);
    if (c !== undefined) this.queue.push(c);
    return c;
  }

  // Drops the first n kept bytes (the header).
  consume(n) {
    while (n > 0 && this.queue.length > 0) {
      const c = this.queue[0];
      if (c.byteLength > n) {
        this.queue[0] = c.subarray(n);
        return;
      }
      n -= c.byteLength;
      this.queue.shift();
    }
  }

  async next(max) {
    if (this.queue.length > 0) return this.queue.shift();
    return this.readSource(max);
  }

  // The next chunk, at most `max` bytes of it; the rest is kept, as a copy
  // so a write of the first part cannot detach it.
  async nextUpTo(max) {
    let c;
    try {
      c = await this.next(max);
    } catch (e) {
      c = undefined;
    }
    if (c === undefined) {
      this.failed = true;
      return undefined;
    }
    if (c.byteLength > max) {
      this.queue.unshift(c.slice(max));
      c = c.subarray(0, max);
    }
    return c;
  }

  // Reads and drops n bytes. False if the body ended first.
  async skip(n) {
    while (n > 0) {
      const c = await this.nextUpTo(n);
      if (c === undefined) return false;
      n -= c.byteLength;
    }
    return true;
  }

  // Writes the next n bytes to `writable` and closes it. If the body ends
  // first, `failed` is set and `writable` is aborted.
  async pump(n, writable) {
    const writer = writable.getWriter();
    try {
      while (n > 0) {
        const c = await this.nextUpTo(n);
        if (c === undefined) throw new Error(ENDED);
        n -= c.byteLength;
        await writer.write(c);
      }
      await writer.close();
    } catch (e) {
      await writer.abort(e).catch(() => {});
    }
  }

  // The next n bytes as a stream of known length, for a put.
  take(n) {
    const fixed = new FixedLengthStream(n);
    this.pump(n, fixed.writable);
    return fixed.readable;
  }

  // The finished file for a put: the pieces at `keys`, each `piece` bytes,
  // opened one at a time, then the next `tail` bytes of the body. `missing`
  // is set when a piece cannot be read or has the wrong size.
  compose(bucket, keys, piece, tail) {
    const fixed = new FixedLengthStream(keys.length * piece + tail);
    const out = { readable: fixed.readable, missing: false };
    const run = async () => {
      for (const key of keys) {
        let ok = false;
        try {
          const object = await bucket.get(key);
          if (object && object.size === piece) {
            await object.body.pipeTo(fixed.writable, { preventClose: true });
            ok = true;
          } else if (object) {
            await object.body.cancel();
          }
        } catch (e) {
          // A failed read; the put fails with it.
        }
        if (!ok) {
          out.missing = true;
          await fixed.writable.abort(new Error(`${key} is missing`)).catch(() => {});
          return;
        }
      }
      await this.pump(tail, fixed.writable);
    };
    run();
    return out;
  }
}
