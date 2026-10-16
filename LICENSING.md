# Licensing

metastratum is licensed under the [MIT License](LICENSE), with one exception
described below. Third-party dependencies (Rust crates, npm packages) keep their
own licences.

This document explains the exception, how the project interprets it, what it
means for anyone distributing metastratum binaries, and how to use your right to
rebuild the agent with a modified version of the LGPL part. It describes the
project's understanding, not legal advice.

## The exception: libvirt's protocol definition

[`crates/mst-virtrpc/proto/remote_protocol.x`](crates/mst-virtrpc/proto/remote_protocol.x)
is libvirt's RPC protocol definition, copied unmodified from libvirt 12.8.0. It
is licensed under the GNU Lesser General Public License, version 2.1 or (at
your option) any later version. The licence text is next to it, in
[`crates/mst-virtrpc/proto/COPYING.LESSER`](crates/mst-virtrpc/proto/COPYING.LESSER).

### How it is used

When `mst-virtrpc` is built, its build script (`crates/mst-virtrpc/build.rs`)
reads `remote_protocol.x` and generates Rust code from it: the protocol's data
types, their XDR encoding and decoding, procedure numbers, and one client method
per procedure. The generated code is written to Cargo's build directory at build
time and is never checked in. It is compiled into `mst-virtrpc`, and through
`mst-libvirt` statically into the `mst-agent` binary.

`mst-agent` is the only binary that contains this code. `mst-controller` and
`mstctl` do not depend on `mst-virtrpc`.

## Our interpretation

- **The LGPL part.** We treat `remote_protocol.x` as "the Library" in the
  LGPL-2.1's terms. We also treat the code generated from it as a work based on
  the Library, and therefore covered by the LGPL too. Whether a protocol
  definition and code derived from it are copyrightable at all is debatable.
  We don't rely on them not being, and take the conservative reading.
- **The MIT part.** Everything else is a "work that uses the Library" and stays
  under MIT: the hand-written client in `mst-virtrpc` and its code generator,
  `mst-libvirt`, `mst-agent`, and the rest of metastratum.
- **Static linking is fine.** `mst-agent` is a statically linked executable.
  LGPL-2.1 §6 allows distributing such a combination under terms of our
  choice, as long as recipients can modify the Library and then relink.
  §6(a) is met by providing the complete source of both the Library and the
  work that uses it. All of metastratum's source is available, so anyone can
  change `remote_protocol.x` and rebuild `mst-agent`. Rebuilding from source is
  relinking: the generated code is regenerated from the modified definition and
  linked into a new binary. No separate object files or special build
  arrangement are needed.

## If you distribute `mst-agent` binaries

1. **Make the corresponding source available.** This means the exact revision
   the binary was built from. `mst-agent --version` prints it, for example
   `mst-agent 0.1.0 (76b8b5297f28)`. You can ship the source with the binary.
   You can instead point to that revision in a public repository from the same
   place the binary is offered (§6(d)). Or you can include a written offer of
   the source, valid for at least three years (§6(c)).
2. **Only distribute builds of a published revision.** A version ending in
   `-modified` was built from a checkout with uncommitted changes, so its
   source is not identified by the revision alone. When building outside a git
   checkout, set `MST_SOURCE_REVISION` to identify the source instead.
3. **Include both licences.** That's [`LICENSE`](LICENSE) (MIT) and
   [`COPYING.LESSER`](crates/mst-virtrpc/proto/COPYING.LESSER) (LGPL-2.1), plus a
   notice that `mst-agent` contains LGPL-licensed code from libvirt. Shipping
   this file covers the notice.
4. **Don't add terms that forbid modifying the binary**, or reverse engineering
   it to debug such modifications (§6). MIT adds no such terms. Make sure any
   terms you distribute under don't either.

## Rebuilding `mst-agent` with a modified protocol definition

This is how you exercise the right to modify the LGPL part and relink.

1. **Find the source revision** of the agent you have:

   ```sh
   mst-agent --version          # e.g. mst-agent 0.1.0 (76b8b5297f28)
   ```

2. **Check out that revision:**

   ```sh
   git clone <metastratum repository> metastratum && cd metastratum
   git checkout 76b8b5297f28
   ```

3. **Modify `crates/mst-virtrpc/proto/remote_protocol.x`.** For example, apply a
   patch, or replace it with the file from another libvirt release (in
   libvirt's source tree it is `src/remote/remote_protocol.x`). Code is
   regenerated from it on the next build. Two cases need more than that:
   - If the new definition uses constants from libvirt's C headers that the
     current one doesn't, add them to `EXTERNAL_CONSTS` in
     `crates/mst-virtrpc/build.rs`.
   - If types used by `mst-libvirt` change shape, adjust
     `crates/mst-libvirt/src/conn.rs` to match.

4. **Rebuild.** You need a Rust toolchain, the musl target and `protoc`:

   ```sh
   rustup target add x86_64-unknown-linux-musl
   cargo build --release --locked -p mst-agent --target x86_64-unknown-linux-musl
   ```

   The new binary is `target/x86_64-unknown-linux-musl/release/mst-agent`. Its
   version shows the revision with `-modified`, since your change is not
   committed.

5. **Optionally, test it.** `cargo test -p mst-virtrpc` checks the generated
   encodings. With `MST_TEST_LIBVIRT_SOCKET=/run/libvirt/virtqemud-sock` (or
   your daemon's socket), it also exercises a live libvirt daemon.

6. **Install it** in place of the old binary and restart the service:

   ```sh
   sudo install -m 0755 target/x86_64-unknown-linux-musl/release/mst-agent /usr/local/bin/mst-agent
   sudo systemctl restart mst-agent
   ```

   The host's enrollment (`/var/lib/metastratum-agent`) is unaffected, so the
   agent reconnects to its controller as before.
