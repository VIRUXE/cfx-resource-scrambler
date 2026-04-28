# cfx-resource-scrambler

A Rust rewrite of [`indilo53/fxserver-resource-scrambler`](https://github.com/indilo53/fxserver-resource-scrambler).

It walks your FiveM resources, finds every `RegisterServerEvent` /
`RegisterNetEvent` / `AddEventHandler` / `Trigger*Event` / ESX callback, and
replaces every event name with a random UUID — so `lua` injectors can no
longer trigger sensitive events by guessing names. A companion `scrambler-vac`
resource is generated that listens for the **original** names and reports any
client that triggers one.

> The best advice in general is to never trust the client and make appropriate
> changes to your resources.

## Why a port

The original Node.js tool had three problems on modern systems:

1. Its only Lua manifest parser is the abandoned `node-lua` C++ binding, which
   does not build on any Node ≥ 12.
2. Two pre-existing bugs in `resourcescrambler.js` made it crash on first
   invocation and, once patched, push single-character event names instead of
   real ones.
3. The rewrite loop is `O(scripts × events × regex_compile)` — on a workload of
   200 resources × 80 events the upstream takes **55 minutes**.

This port keeps the same on-disk behaviour (same `loader.lua`, same call sites,
same `scrambler-events.json` format, same `scrambler-vac` honeypot), parses
manifests via embedded Lua 5.4 (`mlua`, vendored — no system Lua required),
fixes the two upstream bugs, and replaces the inner regex loop with a single
HashMap-backed pass per call site.

## Benchmarks

Same synthetic FiveM workload, same `loader.lua`, both producing valid
scrambled output. Node 10 + patched upstream vs. release Rust binary:

| Workload                  | Files | Bytes  | Node (original) |       Rust |        Speedup |
|---------------------------|------:|-------:|----------------:|-----------:|---------------:|
| 5 resources, 10 events    |    26 |  30 KiB|         0.193 s |   11 ms    |          ~17 × |
| 25 resources, 30 events   |   106 | 376 KiB|         3.929 s |   35 ms    |         ~112 × |
| 75 resources, 50 events   |   306 | 1.8 MiB|       3 min 57 s|  131 ms    |       ~1 800 × |
| 200 resources, 80 events  |   806 | 7.6 MiB|     54 min 58 s |  492 ms    |       ~6 700 × |

A typical FXserver deployment lands well below the largest row.

## Install

### Prebuilt binaries

Download the latest archive from the [releases](../../releases) page:

* `resource-scrambler-linux-x86_64` — static-ish Linux binary
* `resource-scrambler-windows-x86_64.exe` — Windows binary (no Lua DLL needed)

Both bundle Lua via `mlua`'s `vendored` feature, so there is nothing else to
install.

### Build from source

```sh
cargo build --release
```

A C toolchain is needed at build time (for the vendored Lua); none is needed
at runtime.

For Windows cross-builds from Linux:

```sh
apt install gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Usage

1. **Back up your resources first.**
2. Run the binary, passing the path to your resources directory:

   ```sh
   ./resource-scrambler /path/to/your/resources
   ```

3. Move the contents of `./scrambled_resources/` back into your server's
   resource directory and add `start scrambler-vac` to your `server.cfg`.

```
resource-scrambler <resources-dir> [--dst <dir>] [--loader <path>] [--timings] [--quiet]

  <resources-dir>  directory containing the resources to scramble (required)
  --dst <dir>      output directory                        (default ./scrambled_resources)
  --loader <path>  override the embedded Lua manifest sandbox
  --timings        print per-step durations to stderr
  --quiet, -q      suppress per-script progress output
```

The `__resource.lua` manifest sandbox (`loader.lua` at the repo root) is
compiled into the binary, so a normal install needs nothing besides the
executable. The `--loader` flag is for advanced users who want a customised
sandbox — e.g. recognising additional manifest directives.

## Output

* `scrambled_resources/<your resources>/…` — modified resources with scrambled
  event names.
* `scrambled_resources/scrambler-events.json` — mapping of every original event
  name to its new UUID, split into `server`, `net`, and `client`.
* `scrambled_resources/scrambler-vac/` — a generated FiveM resource that
  listens for the **original** event names and reports any client that triggers
  one.

Listen for the alert event server-side:

```lua
AddEventHandler('scrambler:injectionDetected', function(name, source, isServerEvent)
  local eventType = isServerEvent and 'server' or 'client'
  print(('Player id [%d] attempted to use %s event [%s]'):format(source, eventType, name))
end)
```

## Notes

* Always re-scramble all your resources together when you change any of them —
  the new UUIDs are randomized on every run.
* Include the FiveM base resources (and `mysql-async` if you use it) in
  `./resources/` — they're recognised as system resources and their events are
  deliberately left unscrambled.
* This is an experiment. Reports for resources that break after scrambling are
  welcome — please attach the offending script.

## License

Original Copyright © Jérémie N'gadi. Licensed under GPL-3.0 — same as
upstream.
