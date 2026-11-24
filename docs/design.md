# nix-pixie design

Status: draft. Captures decisions made so far plus concrete proposals for the
open parts. Items marked **VERIFY** are assumptions that need a VM test before
we build on them.

## Implementation status

| Area | State | Covered by |
|---|---|---|
| `pixie` metadata (lib + flake-parts module) | done (machines, match hints, pinned-disks file, host-key mode, facter path) | `checks.eval-metadata` |
| Pairing codes, wordlist, layout normalization | done | `nix-pixie-proto` unit tests |
| SPAKE2 pairing with channel binding | done | unit tests incl. relay attack and early-hangup guessing |
| Firewall-free discovery (DNS-SD + neighbour-table probes), controller never listens | done | `checks.vm-pairing` (controller behind the default NixOS firewall) |
| iroh over IPv6 link-local | done, via a fork of iroh (`oddlama/iroh`, branch `nix-pixie-link-local`) until upstream fixes iroh#4584 | `checks.vm-link-local`; regression test in the fork |
| NetworkManager on the operator's machine (direct cable) | behaviour pinned down, mitigation documented | `checks.vm-link-local` |
| Agent service (target-shown code, rotation) | done | `checks.vm-pairing` |
| Controller `id` / `pair` / `serve` (scriptable, no full-screen TUI) | done | `checks.vm-pairing` |
| Controller console: live step list with Nix's progress, log lines above, questions below; plain lines without a terminal | done | unit tests of the rendering and of Nix's log; `checks.vm-install-hooks` (live, on a pty, Enter for the boot check), every other VM test (plain) |
| Installer ISO (`packages.installer-iso`) | builds, boots with UEFI, installs | `checks.vm-iso-uefi` (the image plus the test backdoor, link-local only); BIOS boot of the ISO only by hand |
| UEFI with systemd-boot (EFI system partition, boot entry in the firmware's variables) | done | `checks.vm-iso-uefi` |
| Preflight: boot loader vs the firmware the installer booted with | done | unit tests |
| Inventory, matching against the flake's `pixie` hints, approval/rejection, facter report into the repo (`git add`) | done (scriptable `controller pair`; TUI later) | `checks.vm-inventory` |
| Store push into the target's disk (`--push`, stock Nix worker protocol, never into RAM) | done | `checks.vm-push` (512 MiB under a 192 MiB memory cap; a whole system) |
| Disk selection (pinned, agnostic, fixed), disko, system pushed into the mounted root (`controller install`) | done | `checks.vm-disko`, `checks.eval-disks`, unit tests of the disk plan |
| By-pixie links on the installed system (udev rule from the GPT labels) | done | `checks.vm-disko` (rule on the target), `checks.eval-disks` |
| `nixos-install`, finalize (unmount, `zpool export`), reboot or power off, post-boot host key check | done | `checks.vm-install-*` (boots the installed disks in a second VM; a run where the key doesn't arrive fails with a mismatch) |
| Disk layouts, installed and booted: ext4, ZFS (pool exported, no forced import), btrfs subvolumes, LVM, mdadm RAID 1 on two disks, tmpfs root, LUKS, encrypted ZFS | done | `checks.vm-install-<layout>` |
| Encrypted layouts: LUKS (`passwordFile`, `settings.keyFile`), ZFS native encryption (created from `keylocation=file://`), passphrases typed in on the controller | done, interactive only | `checks.vm-install-{luks,zfs-encrypted}`, unit tests of what is needed and refused |
| Secrets from hooks: disk keys (`$PIXIE_OUT/disk-keys/<path>`, the terminal only for what's missing), host keys (push mode) | done | `checks.vm-install-luks-scripted` (no terminal at all), unit tests |
| Hooks: declared per machine or globally, ordered as a DAG with the install's steps, resolved and built before pairing, files into the installed system; `lib.hooks.agenixRekey` | done | `checks.vm-install-hooks`, unit tests of the ordering, file collection and placement |
| Host keys: pull mode (target generates, public key recorded in the repo before building) | done; no rekeying hook yet | `checks.vm-install` |
| Host keys: push mode (from a hook, stored on the target, checked after boot) | done | `checks.vm-install-luks-scripted` |
| Long sessions across link-local flaps | not verified | needs the store push |
| Targets on other networks (`--target HOST`, unicast probes) running the agent package on a stock NixOS live system (`nix run …#agent`) | done; this way the target must be reachable on UDP 47011/47012 | `checks.vm-remote` (target behind a router with its default firewall; plain NixOS, none of our modules) |
| Targets nobody can connect to (NAT, closed firewall): found at a relay with only the code, then a direct path where one can be punched. n0's public relays by default, `--relay URL` for others, `--no-relay` for none | done for target-shown codes | `checks.vm-relay` (no open ports; first a whole install through the relay with no route at all, then one over a punched path, with the host key checked after boot); by hand through n0's relay between two NATed network namespaces |
| Online guessing: rotation after 3 spent guesses, backing off 2 s doubling to 60 s | done | unit tests of the delay, `checks.vm-pairing` |

## Goal

Take a machine with nothing on it to a fully installed NixOS system defined in
the user's flake. The machine only boots our installer image; at most someone
enters or reads one short code. Everything else happens on the controller.

nix-pixie is a **library with hooks**, not a framework. It works with any
existing `nixosConfigurations`. Common features (facter, agenix-rekey, sops,
host keys) are opt-in. We take good ideas from clan-core without its inventory
or opinions.

## Decisions (agreed)

| Topic | Decision |
|---|---|
| Language / UI | Rust, single binary, ratatui TUI for the controller |
| Nix tooling | flake-parts, treefmt-nix, nix-cargo-integration, numtide devshell |
| Consumers | `flakeModules.default` **and** a plain `lib` usable without flake-parts; generic installer image + controller runnable from our flake |
| Primary boot path | Our installer image (extends the NixOS installer; same kernel/fs support) with the agent autostarted. PXE only serves that image, for convenience. kexec is nice to have |
| Closure transfer | **Push** from controller straight into the target's on-disk store after disko. Never staged in tmpfs. Target can never read the controller's store |
| diskoScript | Built on the controller, pushed as a store path |
| Approval | Controller always approves. Warn on existing partitions/data |
| Upstream cache | Off by default (direct push assumed faster); can be enabled per session in the controller |
| Host keys | Both push (controller provides key) and pull (target generates, controller rekeys) |
| Disk selection | Two modes: pinned (real id written to repo) and agnostic (stable alias path) |
| Facter | Supported (opt-in) |
| Building | Controller only coordinates; building uses whatever the host has (binfmt, remote builders, trebuchet…). Clear errors on arch mismatch |
| Physical steps | Controller enters wait mode and notifies until the user finishes the step |
| Network | mDNS/DNS-SD + probes + iroh, and a relay for targets on other networks; optional DHCP server for direct-cable setups; fully airgapped must work |
| Operator's firewall | Needs no open ports. The controller only dials out (see §1) |
| License | MIT |
| Scope v1 | LAN and airgapped; targets the controller can reach by address (`--target`, e.g. a server on the internet); and targets it can't reach at all, through a relay both connect to (§1) |
| Relays | n0's public iroh relays unless told otherwise; any `iroh-relay` can be used instead, or none |
| Default pairing | Mode 3 (target shows code, operator types it on the controller). Mode 2 (controller shows code) also available |
| Auto-approve | Not in v1. Match hints only pre-select a configuration; a human always approves |
| Name | nix-pixie (binary `nix-pixie`) |

## Components

```
nix-pixie (one binary)
├── controller   TUI, session state machine, eval/build via `nix` CLI, hook runner, store push
├── agent        runs as root in the installer: discovery, pairing, inventory, disko, receive store, install
└── netboot      optional privileged helper: DHCP / proxyDHCP / TFTP / HTTP (dnsmasq subprocess)
```

The controller runs as the **normal user**. Only `netboot` needs root
(ports 67/69, interface config) and runs as a separate process. We don't want
flake evaluation, hooks and secret decryption running as root.

## 1. Discovery and transport

- **Data plane: iroh (QUIC, endpoint = ed25519 public key).** Each connection
  is end-to-end authenticated against the pinned endpoint id. ALPNs:
  - `nix-pixie/ctrl/1`: control messages (length-prefixed postcard), versioned
  - `nix-pixie/store/1`: byte-stream forward to the target's store daemon
  - `nix-pixie/log/1`: log and progress streaming
- **The controller never listens.** Only targets announce themselves, and the
  controller always dials. Pairing roles are about who *owns the code*, not
  who opened the connection (§2). So the operator's machine needs no open
  ports, which matters because we can't change its firewall.
- **Discovery on the local network, two ways, merged**
  (`crates/nix-pixie/src/{discovery,mdns,probe}.rs`). Both carry the same
  `Announcement` (endpoint id, iroh port, pairing state, label;
  `nix-pixie-proto/src/announce.rs`), and so does the third way, for other
  networks ([Relays](#relays-targets-nobody-can-connect-to)):
  - **DNS-SD** `_nix-pixie._udp.local` via `mdns-sd`. On the controller this
    only works if its firewall lets mDNS replies in. A stateful firewall drops
    them: replies to a multicast query come from the responder's unicast
    address and match no connection-tracking entry. The default NixOS
    firewall does exactly that.
  - **Probes** on UDP 47012. The controller sends one probe to the broadcast
    address and to `ff02::1`. Targets answer, and to do so their kernel first
    sends an ARP request or neighbour solicitation, which puts them into the
    controller's neighbour table, even though the controller's firewall drops
    the reply itself. The controller then probes every neighbour by *unicast*
    (read via `ip -json neigh`). Replies to those match the outgoing flow and
    pass any stateful firewall. That's a handful of packets, not a subnet
    sweep, and it needs no privileges.
  - **Cached-neighbour catch:** if the target still has the controller cached,
    it sends no ARP/NS, and the controller learns nothing until the entry goes
    stale (5–10 s). So the agent (root) flushes its neighbour entry for a
    prober before replying, at most once per prober every 10 s. Measured:
    pairing right after a NetworkManager flap went from 8.5 s to 0.4 s.
  - Probes are padded to 512 bytes, so replies are always smaller and a forged
    source can't use targets as amplifiers. Replies echo a random nonce. Targets
    reply at most 50 times a second.
  - The IPv4 probe goes to the subnet's *directed* broadcast, computed from
    address and netmask. Not the broadcast address getifaddrs reports: for
    addresses configured without one (`ip addr add 192.168.1.1/24`, NixOS
    scripted networking), that is the interface's own address.
  - When nothing is found, the error lists the interfaces and what each method
    saw. With `--discovery mdns` and zero records, it says the firewall
    probably drops mDNS replies. A target that was found but not reachable gets
    its own error, with the addresses tried.
- **Dialing** (`crates/nix-pixie/src/controller.rs`, `Dialer`):
  - After the first sighting, wait 600 ms to collect all of the target's
    addresses (they arrive via different probes and DNS-SD). Then dial
    everything except IPv6 link-local, unless link-local is all there is.
    Link-local depends on scope ids, and NetworkManager removes it whenever it
    gives up on DHCP.
  - A pairing attempt that broke before any guess was spent (lost connection,
    timeout) is retried when new addresses show up. Wrong codes, wrong roles
    and wrong versions are final.
- **Airgapped:** nothing here needs anything outside the LAN. The relays
  (next section) are tried and, when out of reach, ignored: `checks.vm-pairing`
  and every install test run that way, since test VMs have no internet. With
  `--no-relay` on both sides iroh contacts nothing but the peer. n0's address
  lookup (which would publish endpoints in DNS) is never used
  (`presets::Minimal`).
- **Names of relays are looked up by the system's resolver** (`getaddrinfo`,
  so `/etc/nsswitch.conf` counts), not by iroh's own (`dns.rs`). iroh's asks
  Cloudflare, Google and Quad9 when the system's name servers don't answer,
  and opens a socket per datagram: where no name server answers at all, an
  agent with the default relays held about 600 sockets at a time (870 seen),
  every half minute. With the usual limit of 1024 open files, pairings and
  approvals that fell into such a burst failed with "Too many open files".
  Now each name is looked up by one thread at a time, however often iroh
  asks, and the same agent stays at about 20 open files
  (`checks.vm-pairing`). Addresses that were found are handed out again for
  30 s, because the system's resolver may need longer to succeed than the 3 s
  iroh waits before it asks again.
- **Direct cable (laptop ↔ switch ↔ server) needs no DHCP for pairing.** The
  installer brings up IPv4 and IPv6 link-local on all wired links (networkd
  `LinkLocalAddressing=yes`). Findings, all in `checks.vm-link-local`:
  - **iroh 1.4.1 can't do link-local**: it drops IPv6 scope ids (upstream
    iroh#4584, open). The fix proposed in that issue works: one helper that
    unmaps IPv4-mapped addresses but keeps the scope id, used at six call sites
    in `socket/transports.rs`, `socket/transports/ip.rs` and
    `socket/remote_map/remote_state.rs`. With it the VM test passes; without it,
    the connection times out. We pull it in via `[patch.crates-io]` from
    `github.com/oddlama/iroh`, branch `nix-pixie-link-local` (on tag `v1.4.1`):
    the fix, a regression test that fails without it, and a commit that
    copies the workspace `[lints]` into the crates, because dream2nix can't
    vendor git crates that inherit them. The branch name has no `/`: dream2nix
    copies the URL-encoded branch from `Cargo.lock` into cargo's source
    replacement, which then doesn't match. The analysis is posted on
    iroh#4584. Once it has `ready-for-pr`, propose the first two commits
    upstream (`git push origin nix-pixie-link-local~1:refs/heads/link-local-scope-id`
    in the fork); the lints commit is only for us. Drop the fork once upstream
    releases a fix. Our discovery knows the scope id (probe replies and mDNS records
    carry the interface they arrived on), so iroh needs nothing else.
  - **NetworkManager's default wired profile** fails the activation every 45 s
    when no DHCP lease arrives, and each time *deletes the link-local address*,
    flushes the neighbour table, and re-adds the address as tentative (usable
    ~2 s later). Pairing survives it. Long installs will see a ~2 s outage
    every 45 s, which QUIC should ride out (**VERIFY** with the store push).
    Recommended profile for a direct cable, which the test shows is stable:
    `nmcli connection modify <profile> ipv4.method disabled ipv6.method link-local`.
  - **The NixOS installer profile enables NetworkManager too.** In our image,
    NetworkManager leaves Ethernet alone (`unmanaged = type:ethernet`) and keeps
    Wi-Fi (`nmtui`), and networkd owns wired links. Otherwise the installer
    would flap the same way.
- **Optional DHCP** (`nix-pixie netboot --dhcp IFACE`): dnsmasq with a per-session
  config. Before serving, send a DHCPDISCOVER probe and **refuse to serve if
  another DHCP server answers**, unless `--force`. A rogue DHCP server on an
  office LAN causes an outage. proxyDHCP mode for PXE on networks that have
  DHCP already.
- **Targets on other networks** (`controller pair|install|serve --target
  HOST`, repeatable): instead of DNS-SD and broadcasts, the controller probes
  only the given addresses by unicast, every 1.5 s. HOST is an IP address or
  a name, looked up before anything else so a typo fails early. The reply is
  the same announcement as on a LAN, so pairing, approval and the install run
  unchanged; the code still authenticates the target, and the controller
  still only dials out. The target must be reachable on UDP 47011 and 47012:
  a server with a public address, or one behind a VPN or port forwarding.
  The probe reply is shorter than the (padded) probe, so an agent on the
  internet is no amplifier. `--target` turns the relays off: the address is
  all it needs.

### Relays: targets nobody can connect to

A target behind NAT, or behind a firewall nobody opened, can't be probed.
What both sides can do is connect *out*, to a relay: an iroh relay forwards
packets between the endpoints connected to it (over HTTPS), addressed by
endpoint id, without being able to read them. So the controller could dial
the target through a relay, if it knew the target's endpoint id. All it has
is the code. (`crates/nix-pixie/src/rendezvous.rs`,
`nix-pixie-proto/src/rendezvous.rs`)

- **The rendezvous.** While a target shows a code, it runs a second iroh
  endpoint whose *secret* key is `SHA-256("nix-pixie rendezvous 1\0" ||
  nameplate)`. The controller derives the same key from the nameplate the
  operator typed, so it knows that endpoint's id, dials it through the
  relays, and reads one short stream: the target's announcement, the same
  one DNS-SD and probes carry. From there on nothing is new: `Discovery`
  merges it as a third source, the controller dials the announced endpoint
  (through the relays, and at any addresses the LAN methods found), and
  SPAKE2 decides whether it is the right machine.
- **Nothing but a relay is needed.** No mailbox server, no DNS or pkarr
  records, no DHT. So one self-hosted `iroh-relay` replaces n0's entirely.
- **Unauthenticated on purpose.** The rendezvous key is derived from the
  public half of the code, so anyone can sit at a rendezvous and answer.
  That is a forged announcement, as on a LAN: the controller dials the
  wrong machine, pairing fails, and after 3 such machines it stops and says
  that someone may be impersonating targets. The secret words take no part,
  so neither the relay nor an impostor learns anything that helps guessing
  them.
- **Six-digit nameplates.** On a LAN a target sees which nameplates are
  taken; at a relay nobody can list them, so a random one has to be unlikely
  to collide: 100000 to 999999. A relay hands an endpoint id to whoever
  connected under it last, so two targets with the same nameplate would
  displace each other (the controller keeps asking and tries everyone who
  answers). A target without relays keeps the short ones (1–99), and the
  number of digits tells the controller where to look: for a short
  nameplate it contacts no relay at all.
- **Which relays.** n0's public ones unless told otherwise. `--relay URL`
  (repeatable) on `agent` and on `controller pair|install`, or
  `services.nix-pixie-agent.relay.urls`, names others; `--no-relay` or
  `relay.enable = false` uses none. Both sides need the same setting,
  because the controller asks at every relay of *its* setting. A target
  with other relays than the default prints them with its code:
  `nix-pixie controller pair --code … --relay https://…`.
- **What a relay learns:** who talks to whom (endpoint ids, IP addresses,
  volume, timing), and that it is nix-pixie (the protocol name is in the
  QUIC handshake, which is only protected against observers who don't see
  its first packet). Not the code's words, the inventory, keys or the
  system: all of that is inside the end-to-end encrypted connection. It can
  also see which nameplates are waiting, and so can anyone willing to try
  all 900,000; what they get is a machine to guess codes at, three per code
  with growing pauses (§2).
- **No relay in reach** (airgapped, or a captive network): the code shows
  right away, the agent logs after 15 s that only controllers on its network
  will find it, and keeps trying. The controller's error lists the relays
  next to DNS-SD and probes: `relay: this machine reached none of n0's
  public relays`, or `no target waiting at …`.
- **Direct paths.** Through the relay the two exchange their addresses and
  send to each other at the same time, which opens NAT mappings and stateful
  firewalls for the answers; the connection then moves to that path. The
  approval shows which it is (`Paired with web1 (relayed through
  https://…)`, or the address), the JSON result has `route` (`direct` or
  `relay`), and before the system is sent the controller warns if it would
  still go through the relay. n0's relays throttle (seen from behind NAT:
  "the relay is rate-limiting this endpoint"), so an install through one is
  slow. With an own relay that is your bandwidth to spend.
- **After the install** the controller looks for the installed system's
  SSH server where it found the installer (§3). A target found only at a
  relay has no such address. The one a direct path leads to may be a NAT's,
  and whatever answers SSH there is another machine: a false "host key
  mismatch" at best. So the controller asks the target for its interfaces'
  addresses (`RequestAddresses`) and checks the host key at the path's
  address only if the target has it itself, as a server with a public
  address does. Otherwise it doesn't wait, says why, and prints the key to
  expect (`hostKey.skipped` and `hostKey.expected` in the result); hooks
  after `boot` don't run then.
- **Controller codes** (mode 2) have no nameplate, so `controller serve` and
  `agent --controller-code` use no relay ([Future ideas](#future-ideas)).

### Why not magic-wormhole itself

The relay rendezvous is the wormhole idea (a public nameplate to meet at,
secret words for the PAKE) on infrastructure we need anyway. The protocol
itself didn't fit:

- It needs its mailbox server, which by default is on the internet. That
  breaks airgapped installs; on a LAN, DNS-SD and probes need no server.
- Wormhole codes are single-use per pair. The controller code is reused by
  many targets.
- We would still need iroh for data and a channel binding between the two
  protocols.

What we take from it: SPAKE2, a numeric nameplate plus words, tab-completion
against the wordlist, and rotating the code on failures.

## 2. Pairing and trust (concrete proposal)

### Identities

- **Controller:** a long-term iroh key in `$XDG_STATE_HOME/nix-pixie/identity`.
  The public key can go into the flake (needed for baked images).
- **Target agent:** a fresh key generated each boot, in RAM only.

### Pairing modes

All modes end in the same state: both sides have pinned each other's endpoint
id, then the operator approves on the controller.

1. **Baked image (zero-touch, strongest).** The image or the PXE kernel command
   line (`pixie.controller=<endpoint-id>[@addr]`) contains the controller key.
   The target dials it and authenticates the controller via iroh. Possession of
   the image counts as enrollment, so the operator still approves based on the
   hardware fingerprint. PXE-served images get this automatically, because the
   controller writes the kernel command line.
2. **Controller code (your original flow).** The controller shows
   `river-candle-moss`. The operator types it on the target, which then
   announces "I typed a controller code". The controller finds such targets
   and dials them, and runs SPAKE2 as the code owner.
3. **Target code (default when the target has a screen).** The target
   shows `482913-river-candle-moss`. `482913` is a public nameplate, in the
   target's announcement and the name of its rendezvous at the relay; the
   words are secret. The operator types it into the controller TUI.
   Benefits:
   - The target needs no keyboard.
   - The code is single-use and tied to exactly one machine.
   - The operator types on their own keyboard layout (see below).
4. **Headless with no screen:** only baked or PXE mode. With a plain generic
   image and no screen, nothing can authenticate the machine. That would be
   trust on first use plus approval, which we should offer only behind an
   explicit flag and a warning.

### PAKE details

Implemented in `crates/nix-pixie-proto/src/pair.rs`.

- `spake2` crate, symmetric mode, with identity `nix-pixie/pair/1`. The
  password is the canonical spelling of the whole code.
- **Channel binding is mandatory.** The PAKE runs *inside* an iroh connection
  whose encryption keys don't come from the PAKE. Without binding, an attacker
  holding two iroh connections can relay the SPAKE2 messages: both ends
  "succeed" while each one is actually talking to the attacker. Fix: the key
  confirmation is `HMAC(confirm_key(K), sender_role || version ||
  ctrl_endpoint_id || target_endpoint_id || both SPAKE2 messages)`, with the
  endpoint ids as iroh authenticated them on each side. The sender's role
  stops reflection. A unit test relays a full exchange and requires both
  sides to fail; with the ids removed from the MAC, that test fails.
- **The typist confirms first.** The side that owns the code sends its MAC
  only after verifying the typist's. Otherwise an attacker could send a guess,
  receive the owner's MAC, check the guess, and hang up. That would look like a
  dropped connection, not a wrong code. With this order, every real guess ends
  in a `Mismatch` on the owner, which is exactly what it counts. Who dials is
  independent of this: the controller always dials.
- **Both sides have a guess budget.** The mirror image of the above: a rogue
  *owner* sees the typist's MAC first and can check one guess, then hang up.
  So `pair` reports `guess_spent`. The owner sets it on a wrong confirmation.
  The typist sets it as soon as its MAC is out, whatever happens next (tracked
  across the timeout, which `pair` applies itself). After 3 spent guesses, the
  owner rotates its code, and the typist gives up and asks for the code again.
  Unit tests cover each case, including that an early hangup or a stalled peer
  doesn't count.
- **Wordlist:** EFF Short Wordlist #1 (CC BY 3.0 US) minus `yo-yo`. That's
  1295 words of 3–5 letters, so 3 words ≈ 31 bits. The PAKE makes this
  online-only: one guess per attempt. The code owner rotates after 3 wrong
  guesses and logs an alert ("someone may be guessing"). Rotating rather than
  locking out avoids a permanent DoS. Timeouts and protocol errors don't count,
  since they reveal nothing.
- **Backing off between codes.** On a LAN, guessing needs a machine on that
  network. An agent on the internet (`--target`) or at a relay can be
  dialled by anyone, and
  rotation alone only bounds guesses by how fast pairing round trips go. So
  the owner waits before showing the next code: 2 s, doubling up to 60 s.
  That leaves 3 guesses a minute, about 1 in 500,000 per day of the agent
  running. An operator who mistyped three times waits 2 s.
- **Nameplates** are 1–99 for a target without relays, which surveys DNS-SD
  for 2 s and picks an unused one, and six digits for a target that also
  waits at a relay (§1). Collisions are still possible, so the controller
  tries every target with that nameplate.
- **No emoji SAS.** The Linux VT (fbcon) can't render emoji. With PAKE plus
  approval a SAS adds nothing, and mode 3 already covers screen-without-
  keyboard machines.
- **Keyboard layouts.** The installer console defaults to US. On QWERTZ, y and
  z are swapped; AZERTY also swaps a/q and w/z. Mitigations:
  - Tab-completion against the wordlist.
  - Normalizing layout confusions before the PAKE (map the input through
    QWERTZ/AZERTY inverses and keep the variant made of valid words).
    **Implemented:** a unit test checks that every word, typed on US, QWERTZ
    or AZERTY, normalizes back to itself. The EFF list needed no removals for
    this. Separators can be any non-alphanumeric character (except `;`, which
    is AZERTY `m`), because `-` moves between layouts.
  - A layout switch key on the target UI.

  This is another reason to prefer mode 3.

### Trust consequences

- The controller sends secrets to the target only after pairing **and**
  approval.
- MAC, DMI serial and similar matches are **hints** that pre-select a
  configuration. They never authenticate. There is no auto-approve in v1 (see
  [Future ideas](#future-ideas)).
- **All data from the target is untrusted:** facter JSON, disk list, logs,
  host pubkey.
  - Enforce size limits.
  - Validate disk ids (`^[A-Za-z0-9._:+-]+$` under `/dev/disk/by-id`), since
    they end up in generated disko shell scripts and in the repo.
  - Strip control characters from anything rendered in the TUI.

## 3. Session flow

```
target boots ─► network (DHCP | IPv4LL | IPv6LL) ─► DNS-SD ─► pair ─► APPROVE
  ─► inventory ─► select config + disks + options
  ─► [pull] keygen in tmpfs ─► hooks (rekey, write repo files) ─► build ONCE
  ─► preflight ─► CONFIRM (destructive) ─► push diskoScript closure (tmpfs, small)
  ─► disko ─► start /mnt store receiver ─► PUSH toplevel into /mnt
  ─► extra files ─► nixos-install --system ─► [wait-mode hooks] ─► finalize ─► reboot
  ─► post-boot verification
```

### Notes on the flow

- **Inventory** (implemented: `nix-pixie-proto/src/inventory.rs`, collected by
  `crates/nix-pixie/src/inventory.rs`), sent right after pairing:
  - system double, CPU, RAM, hostname
  - firmware: UEFI/BIOS, Secure Boot, setup mode (efivars)
  - DMI vendor/product/serial/uuid. Known placeholder values ("To Be Filled
    By O.E.M.", ...) are dropped, so they can't make unrelated boards match
    the same `dmiSerial` hint.
  - physical NICs with MACs and link state
  - disks from `lsblk --json`: size, model, serial, transport, `/dev/disk/by-id`
    names (only ones passing `is_valid_disk_id`, since they end up in disko
    configs), partition table, filesystem signatures on the disk and
    everything stacked on it (partitions, LUKS, LVM), and whether anything on
    it is mounted or used as swap (`in_use`, never installable: the boot
    medium, for one)
  - the facter report only on request, after approval, and only for machines
    with `facter.reportPath`
- **Matching:** the controller evaluates `<flake>#pixie` *before* pairing, so a
  broken flake or an unknown `--machine` doesn't use up the code. MAC and DMI
  serial hints suggest a machine; a single match is pre-selected, several are
  reported as ambiguous. `--machine` overrides, and the summary says when it
  contradicts the hints.
- **Approval:** the operator sees a summary (sanitised again on the controller)
  and answers y/N (`--yes` in scripts). `Reject` sends the target back to a
  fresh code. Approval may take as long as the operator needs; iroh's
  keep-alives hold the session (tested with 40 s).
- **Delivery of the last message:** the controller finishes its send stream
  and waits for the peer's acknowledgement before closing. Closing right after
  a write discards data still in flight, which lost the `Reject` in the first
  version.
- **Reinstall detection (opt-in).** Mount existing filesystems read-only
  (ext4 `ro,noload`, xfs `ro,norecovery`, zpool `import -o readonly=on -N`)
  and read `/etc/hostname` and `/etc/ssh/ssh_host_ed25519_key.pub`.
  - If the pubkey matches a machine known in the repo, pre-select that machine.
  - In pull mode, offer to **reuse the old host key**, which avoids rekeying.
- **Generate keys before disko, not after.** In pull mode the target
  generates the host key in tmpfs (a few bytes) right after approval. The
  controller then rekeys and builds **once**. The disk is wiped only after
  everything is built, and there's no second build/push round. The agent
  copies the key into `/mnt` after disko.
  - Implemented (`hostKey.mode = "pull"`): `ssh-keygen -t ed25519` on the
    target, the public key checked strictly on the controller and written
    to `hostKey.publicKeyPath` (`git add`) before the configuration is
    evaluated. The key goes to the first ed25519 path in the configuration's
    `services.openssh.hostKeys` (so impermanence setups work), 0600, right
    before `nixos-install`. Rekeying hooks come with the hooks interface.
- **Preflight (fail before wiping):**
  - `hostPlatform.system` of the config == target arch (implemented)
  - no other disk carries partition labels or pool names the layout creates
    (implemented, see Disks)
  - bootloader vs firmware (systemd-boot/lanzaboote config on a BIOS machine
    can't boot). Implemented: systemd-boot, lanzaboote, EFI-only GRUB and
    `canTouchEfiVariables` are refused when the installer booted with BIOS;
    BIOS-only GRUB is refused when it booted with UEFI (it would need CSM).
  - lanzaboote with Secure Boot not in setup mode → plan a wait step
  - number of disks the disko config needs vs selected
  - fixed partition sizes vs disk size
  - closure size (`nix path-info -S`) vs the expected `/mnt` capacity
  - airgapped: `nix build --dry-run --offline` shows nothing to fetch
- **Destructive confirm:** warn if partition tables, fs signatures or a
  detected NixOS install are present (see [Order of operations](#order-of-operations)).
- **Clock.** Airgapped hardware often has a wrong RTC (dead CMOS battery). Over
  the authenticated control channel the controller sends its time and the
  agent sets the clock. Otherwise TLS certificates (nix-grpc mTLS) fail with
  "not yet valid", and the installed filesystems get wrong timestamps. iroh
  uses raw public keys, so it isn't affected.
- **Finalize:** run `zpool export` for every pool disko created (otherwise the
  first boot needs a force import), unmount, then reboot, power off or kexec.
  - Implemented: `--then reboot` (default), `poweroff` (e.g. to remove the
    installer medium) or `stay` (everything stays mounted for inspection).
    The agent syncs, unmounts the root recursively, exports the
    configuration's `disko.devices.zpool`s, and shuts down once the
    controller has disconnected (at most 30 s later). kexec isn't done.
- **Post-boot verification:** wait for the host and compare `ssh-keyscan`
  against the expected host key (known in both push and pull mode). Optional
  `post-reboot` hooks.
  - Implemented: after closing the session, the controller asks every
    address the target had for its ed25519 key (`--boot-timeout`, default
    600 s), on the configuration's first `services.openssh.ports` entry. It
    asks at a fixed pace (`--boot-check-interval`, default 30 s), once more
    at the deadline, and right away when the operator presses Enter, after
    which the pace counts anew. No retry loop and no backoff: a machine that
    stays powered off until somebody pulls the USB stick isn't asked
    hundreds of times meanwhile, and there is no backoff to wait out once
    it is up. Every check is logged; the step counts down to the next.
    `Finished` carries the installer's own host key, which is ignored, since
    it may still answer while the target goes down. Any other key than the
    expected one fails at once with HOST KEY MISMATCH. In mode `none` the
    new key is reported, unverified. Skipped if the configuration doesn't
    enable sshd. The result is `hostKey` in the JSON; exit status 1 if the
    check fails. For targets found only at a relay, see
    [Relays](#relays-targets-nobody-can-connect-to).
- **Persistence:** a session journal (no secrets) in
  `$XDG_STATE_HOME/nix-pixie/sessions/`. A controller restart resumes. The
  agent reconnects using the pinned keys, and store pushes are idempotent per
  path.

### The operator's terminal

Implemented in `crates/nix-pixie/src/ui/`, for every controller command.

- **Live**, when stderr is a terminal and `TERM` isn't `dumb`: log lines
  scroll by above a region at the bottom that is redrawn in place. It shows a
  rule, the steps, another rule, and what the operator is told or asked right
  now. The steps are known from the start (`□`), spin while running and end
  as `▣` (done), `✗` (failed) or `–` (turned out not to apply, e.g. no
  address to check the first boot at), with how long they took and a detail
  on what they do or did. The list is refined as more is known: the
  machine's optional steps and its hooks once it is approved, disk keys and
  the boot check once its configuration is evaluated.
- **Questions** are asked at the bottom of the region, with the cursor after
  them. Once answered, what was shown with the question, the question and the
  answer move up into the scrollback, which so reads as a transcript of log
  lines and decisions. At the end the region stays on the screen as the
  final summary, and the JSON result follows below it.
- **Plain** for pipes, files and the journal: a line per step as it starts
  (`▸ Send the system (into /mnt, zfs)`), everything said printed as it
  comes, no colour, nothing redrawn, and Nix's own output on stderr as
  before. The VM tests run this way, except `vm-install-hooks`, which runs
  the controller on a pty with `TERM` set and presses Enter for the boot
  check.
- **One owner of stderr.** Tracing writes through the console
  (`ui::log_writer`, installed by `main.rs`): a log line clears the region,
  is printed, and the region is drawn again below it. A thread draws the
  spinner and reads stdin. Step lines are cut to the terminal's width and
  what is said is wrapped, so every row is accounted for; the region never
  takes more than the terminal's height minus a row, and a long list shows a
  window around the running step. Redraws use synchronized output where the
  terminal knows it.
- **stdin is never read blocking.** A blocking read can't be cancelled: the
  process couldn't exit while one waits, and it would take the next line
  from a hook that reads the terminal itself. stdin is polled, and read only
  while somebody waits for a line, or to drop a line typed while nobody
  asked. On a terminal that line was echoed into the region (which is redrawn
  a row lower), and a stray `y` mustn't answer the next question; before a
  question, what was typed ahead is flushed as well. Input from a pipe or a
  file is a script's answers and kept in order. Only the foreground process
  drops input; in the background, reading the terminal would stop it.
- **Hooks and passphrase prompts** get the terminal to themselves
  (`ui::suspend`): the region is cleared and neither drawn nor is stdin read
  until they are done. A hook's output follows a line naming it.
- **Nix's progress.** On a live console, `nix build` and `nix copy` run with
  `--log-format internal-json` and stdin from `/dev/null` (so Nix never asks,
  e.g. about a flake's `nixConfig`). Their activities add up to the running
  step's detail the way Nix's own progress bar adds them up (`actBuilds`,
  `actCopyPaths`, the bytes of `actCopyPath`, from `resProgress` and
  `resSetExpected`), followed by the newest build, copy or download. Errors
  are printed as Nix wrote them, warnings become log lines, everything else
  is logged at debug level.
- **Why not a full-screen TUI (yet).** An alternate screen gives up the
  scrollback, which is the transcript, and copy and paste with it. It would
  have to be left for every hook that needs the terminal (agenix-rekey with
  a YubiKey PIN) and every passphrase prompt, and it is a second front end
  to keep apart from the scriptable one. The inline region gets the steps,
  the progress and the questions where the operator looks, and degrades to
  plain lines by itself. A full-screen view earns its keep once several
  machines are installed at once.

## 4. Store push (no tmpfs, no pull)

### Implemented: stock Nix worker protocol (no plugin)

`crates/nix-pixie/src/{push,store}.rs`, `checks.vm-push`.

- **Controller:** after approval, `PrepareStore { root }` asks the target to
  check `root`. Then `nix copy --no-check-sigs --to unix://<socket>` runs
  against a socket in a fresh 0700 directory (`$XDG_RUNTIME_DIR` or `/tmp`).
  Every connection on it becomes a QUIC stream on the paired session, starting
  with a `StreamRequest::Store { root }` header. `--no-check-sigs`: paths the
  controller built itself carry no signatures, and the target trusts the
  paired, approved controller. The socket's directory keeps other local users
  away from the target's root-level daemon.
- **Target:** for each stream (only after approval), `nix --store
  local?root=<root> daemon --stdio --force-trusted` (needs the
  `daemon-trust-override` experimental feature), piped through fixed small
  buffers. QUIC flow control keeps the controller from getting ahead of the
  disk.
- **Never into RAM:** `check_root` finds the filesystem that would hold
  `<root>/nix/store` in `/proc/self/mountinfo`. It refuses if that filesystem
  isn't mounted under `root` (e.g. `/mnt` not mounted, so the store would land
  in the live system), or is tmpfs, ramfs, overlay, squashfs or similar.
- **Verified** (`checks.vm-push`): the agent's service is capped at
  `MemoryMax=192M` and receives a 512 MiB path plus `hello`'s closure, never
  restarted. Its memory peak sits at the limit, which is page cache from the
  disk writes being reclaimed. A mutation that makes the agent hold 300 MiB
  triggers the OOM killer and fails the test, so the setup does catch
  buffering. Also pushes a whole NixOS system: 570 paths, 1.5 GiB in about
  30 s between test VMs (virtual disks and network).
- Checked before pairing: every path to push is valid in the local store.

### Later: nix-grpc-store

Worth adding as a second transport if the worker protocol's per-path round
trips hurt (high RTT, e.g. WAN). On a LAN they don't (see the numbers above).

- **Target:** `nix daemon --store local?root=/mnt` on a private socket, with
  `nix-grpc-daemon --proxy-socket … --client-ca <session CA> --trust-clients`
  in front.
- **Controller:** the iroh stream forwarded to a local TCP port; `nix copy --to
  'grpc://127.0.0.1:P?…'` using our own `nix` wrapped with the plugin
  (`nix-grpc-store.lib.wrapNix`), independent of the user's Nix version.
- mTLS on top of iroh, because the controller-side forward is a localhost TCP
  port that other local users could reach.
- **Gains:** batched path-info queries, pipelined NAR transfer, zstd.

### Restricted to one closure

Push means the target never gets read access to the controller. It receives
exactly the closures the controller sends: the diskoScript closure into
tmpfs, and the toplevel plus `pixie.extraClosures` into `/mnt`.

### Optional upstream substitution (per-session toggle)

The target first runs `nix copy --from https://cache.nixos.org --to /mnt
<paths>`, checking signatures with the config's `trusted-public-keys`. Then
the controller pushes the full closure; paths already valid are skipped.

### Still to verify

- `nix-grpc-daemon` doesn't buffer whole NARs (once that transport exists; the
  stock daemon is verified above).
- Paths larger than the test's 512 MiB (multi-GiB container images) on real
  hardware.
- Lix and Determinate Nix as the controller's Nix (`nix copy` runs with the
  user's Nix).
- The installer's Nix writes the `/mnt` store DB. Confirm the DB schema works
  with older Nix and Lix on the installed system. nixos-install has the same
  property, but check it anyway.

### Install

`nixos-install --system <toplevel> --no-root-passwd --no-channel-copy`. The
path is already valid in `/mnt`, so it only sets the profile and runs
`switch-to-configuration boot` in the chroot. Verified in `checks.vm-install`:
the installed system's store holds exactly the system's closure, so nothing
was copied again or added. The chroot bind-mounts `/dev`, so the by-pixie link
resolves for `grub-install`.

## 5. Disks

Implemented in `crates/nix-pixie/src/{plan,system,install,disko}.rs`,
`nix/nixos/disks.nix`, `lib.pinnedDisk`; tested by `checks.vm-disko`.

The configuration's own `disko.devices.disk` says which disks exist; there
is no separate role list that could disagree with it. The controller
evaluates each disk's `device` (unset ones as `null`) and classifies it:

| Device in the disko config | Mode | What happens |
|---|---|---|
| `nix-pixie.lib.pinnedDisk ./disks.json "main"`, and the machine has `disks.file` | pinned | The operator chooses (or keeps the recorded disk). The controller writes `{ "main": "/dev/disk/by-id/…" }` to `disks.file`, `git add`s it, re-evaluates, and checks that the configuration now uses it. |
| `/dev/disk/by-pixie/<role>` | agnostic | The operator chooses; the agent links the disk there before disko. One configuration installs many machines. |
| anything else (`/dev/disk/by-id/…`, `/dev/<name>`) | fixed | Must exist on the target. Not choosable. |

### Choosing

- `--disk ROLE=DISK`: DISK is a kernel name (`nvme0n1`, `/dev/nvme0n1`), a
  `/dev/disk/by-id` name, or a serial number. Roles are checked before
  pairing when `--machine` is given.
- Order: fixed devices, then `--disk`, then a pinned disk keeps its
  recorded disk, then a last open role with exactly one free disk takes it.
  Anything else asks on a terminal; under `--yes` it fails with the list of
  candidates instead of guessing.
- Never chosen: disks that are mounted or swapped (the installer's own boot
  medium among them), read-only, or empty card readers. Exception: disks
  nix-pixie formatted earlier during this boot of the installer. What is
  mounted from them is that attempt's, and disko's destroy step unmounts it,
  so reinstalling in the same session works. The agent records them in its
  tmpfs state directory.
- Refused before building: a disk that is not being formatted but already
  carries a GPT partition label or ZFS pool name the layout creates (e.g. a
  disk from an earlier install with the same layout). disko finds its
  partitions and pools by name, so it could mount the old partition: the
  system would land on the wrong disk and the boot loader on the new one.
  `checks.vm-disko` hit exactly that before the check existed. The agent
  repeats the check right before disko, in case a disk was attached since
  the inventory.
- Pinned mode records the most descriptive by-id name (model and serial)
  over opaque ones (`wwn-`, `nvme-eui.`). A disk without any by-id name
  can't be pinned; use agnostic mode.

### Order of operations

Everything that can fail without touching the disks happens first:
evaluate, choose, record pins, build the disko script **and** the system.
Then the operator sees exactly what gets erased:

- only empty disks: `y/N`, or `--yes`
- any disk with a partition table or signatures: `y/N` as well, since the
  operator approved this very machine moments ago and typing its name again
  added friction, not safety. Its contents are listed. Without a terminal
  it takes `--wipe`; `--yes` alone refuses.

Then the disko script (`system.build.diskoScript`, which destroys without
prompting) goes into the installer's live store, the agent re-checks every
disk against a fresh lsblk (defence in depth against a stale inventory or a
confused controller) and links agnostic roles, runs disko with its output
streamed back (the tail is shown on failure), and the system is pushed into
`disko.rootMountPoint`.

### The by-pixie path after the install

- Some options keep using the disk path after the install. The main one is
  BIOS GRUB: disko puts the device of a disk with an EF02 partition into
  `boot.loader.grub.devices`, so every `nixos-rebuild` runs `grub-install`
  on it (checked in `checks.eval-disks`).
- `nixosModules.default` (installed system) recreates
  `/dev/disk/by-pixie/<role>` with a udev rule. A helper recognises the disk
  by its partitions' GPT labels (the kernel's `PARTNAME`), using the labels
  disko actually assigned. They are evaluated, never reconstructed, since
  disko hashes labels longer than 36 characters.
- Disks without a GPT can't be recognised; an assertion says so.
- Two disks with the same labels (e.g. one moved over from another machine
  with the same configuration) both claim the link, and udev picks one.
  Documented, not detected.
- **VERIFY** layouts beyond plain GPT + ext4: ZFS vdevs, mdadm, LUKS. disko
  mounts by partlabel, so they should work.

### Encryption

disko reads encryption keys from files on the target (LUKS `passwordFile`
and `settings.keyFile`, ZFS `keylocation = "file://..."`) or asks on its own
terminal (`askPassword`, `keylocation = "prompt"`). The agent runs disko
without a terminal, so only files work:

- The configuration's facts list its LUKS devices and ZFS key locations;
  the controller works out which key files disko reads. Each must be under
  `/tmp` or `/run`.
- Refused, before pairing when `--machine` is given, each with what to
  configure instead: `askPassword` (use `passwordFile`; when *creating* a
  device disko always reads the passphrase from its stdin, so the agent
  can't answer it, and feeding stdin would also feed other tools' questions,
  e.g. mdadm's), `keylocation = "prompt"` (create from a file and switch
  with `postCreateHook = "zfs set keylocation=prompt <dataset>"`, as in
  disko's own example), `additionalKeyFiles` and raw or hex ZFS keys (not
  typeable).
- Keys come from hooks first: `$PIXIE_OUT/disk-keys/<path>` is the key
  disko reads at `<path>` on the target, from hooks that run up to `disks`.
  Those can be what can't be typed: LUKS `additionalKeyFiles`, raw (32
  bytes) and hex (64 digits) ZFS keys; each is checked for what reads it,
  and keys nothing reads are dropped, not sent. Whatever no hook provided,
  the controller asks for on the terminal, twice and without echo (ZFS: 8
  to 512 characters), after the disk plan and before the build, so the
  build runs unattended. Without a terminal, missing keys are an error;
  without hooks, that is known before pairing.
- `RunDisko` carries the key files. The agent writes them 0400 only to tmpfs
  or ramfs (checked against mountinfo), so they never touch a disk, runs
  disko, and removes them whatever the outcome. `KeyFile` zeroizes its
  contents on drop and never shows them in `Debug` output.
- The installed system asks for the passphrase on its console at boot; the
  controller says so while it waits for the host key.

### Facter in fleet mode

Facter reports are per machine, so they conflict with "one config for many
machines". In agnostic fleet configs, either skip facter (generic
`hardware-configuration`) or store per-machine reports and pick one by match
rule. That's a documented limitation, not a bug.

### Writing to the repo

- pixie writes only to paths declared in config (data files, never `.nix`).
- It runs `git add` on them, because flakes ignore untracked files.
- It shows the diff in the TUI and **never commits**.

## 6. Hooks

Implemented in `crates/nix-pixie/src/hooks.rs`, declared in `nix/options.nix`;
tested by `checks.vm-install-hooks` and unit tests.

Hooks are programs from the user's flake that **run on the controller**, in
the repository, at points of the install.

- **Declaration:** `hooks.<name>` for every machine and
  `machines.<name>.hooks.<name>` per machine. A machine's hooks get the
  global ones as defaults, option by option, so it can change one setting
  or drop a hook with `enable = false`. Each has `command` (a package of the
  flake, by name or attribute path, or any installable with `#`), `args`,
  `after` and `before`.
- **Ordering: a DAG, not phases.** `after` and `before` name other hooks or
  the install's own steps, which are nodes of the same graph in their fixed
  order: `approve`, `facter`, `host-key`, `disks`, `build`, `format`,
  `copy`, `install`, `finish`, `boot`. So `after = [ "host-key" ]; before =
  [ "build" ]` places a hook between the two, and hooks can order each
  other. Every hook comes after `approve` (it needs an approved target) and
  runs as early as its constraints allow; ties go by name. Unknown names,
  hooks named like steps and cycles are refused before pairing.
- **Resolved once, before pairing:** each hook is evaluated to its
  derivation and main program (like `nix run` and `lib.getExe` pick it),
  and all are built together. A broken hook doesn't cost the pairing code,
  running one later costs no evaluation, and a hook that changes the
  repository can't change what other hooks run. Hooks must be packages,
  not apps.
- **Contract:**
  - cwd: the repository (the local flake). Hooks that change it must
    `git add` what the build needs, as `agenix rekey --add-to-git` does.
  - stdin: the operator's terminal, not data: rekeying may need a
    YubiKey PIN or touch. The live console steps aside while a hook runs.
    stdout goes to the controller's stderr, since the controller's stdout
    is its JSON result.
  - `$PIXIE_CONTEXT`: a JSON file with the hook's name and step, the
    machine, configuration and flake, the target (id, label, inventory),
    and what is known so far: the pulled host key (from `host-key`), the
    disk plan (`disks`), the system (`build`), the mounted root (`format`
    to `install`).
  - `$PIXIE_OUT`: an empty directory for what the hook produces (anything
    else in it is refused as a typo):
    - `disk-keys/<path>`: keys for disko (see Encryption), from hooks up to
      `disks`.
    - `host-key`: push mode's private host key (OpenSSH, no passphrase),
      from a hook before `host-key`. The agent keeps it where a generated
      key would be and derives the public half with `ssh-keygen -y`; from
      there it is pull mode (recorded, installed, checked after boot).
    - `files/<path>`: into the installed system at `<path>`, root-owned
      with their permission bits (never setuid, setgid or sticky), at most
      15 MiB in all.
    Outputs that come too late (disk keys after `disks`, a host key after
    `host-key`), two hooks providing the same key, and a host key when the
    mode isn't push are errors.
  - Files for the installed system from before `format` wait until the
    root is mounted; after `finish` there is no target any more, and
    producing files is an error.
    The agent refuses paths that leave the root, also through symlinked
    directories inside it (NixOS's `/etc/static`).
  - Both live in a private directory, removed after each run.
  - A non-zero exit aborts the install. The JSON result lists every hook,
    its step and whether it ran (`--then stay` and systems without sshd
    never reach `finish`/`boot`).
- **Secret hygiene:** hook output never goes through a Nix build (which
  would put it in a world-readable store), is never logged, and never
  touches the target's `/nix/store`. Placed files are zeroized in memory
  and redacted from `Debug` output.
- **Presets:** `lib.hooks.agenixRekey { command ? "agenix-rekey"; }` runs
  `agenix rekey --add-to-git` after `host-key` and before `build`; the
  command is a package in the user's flake, e.g. `config.agenix-rekey.package`
  from agenix-rekey's flake-parts module. The configuration reads the
  pulled key with `age.rekey.hostPubkey = nix-pixie.lib.agenixRekey.hostPubkey
  ./<hostKey.publicKeyPath>;`. Until the first install recorded it, that
  leaves agenix-rekey's own default (a dummy key it warns about), so the
  configuration evaluates and builds before, like `lib.pinnedDisk` for
  disks.
- **Not yet:**
  - helpers on the hooks' PATH: `nix-pixie target exec [--chroot] -- …` (run
    a command on the target, e.g. `systemd-cryptenroll`, `sbctl
    enroll-keys`) and `nix-pixie wait "…"` (wait mode with a notification)
  - a `result.json` (e.g. asking for a rebuild)
  - more presets: facter, sops

## 7. Installer image

**Built:** `packages.<system>.installer-iso` = `installation-cd-minimal` +
`nixosModules.installer` (agent, wired networking via networkd, firewall for
UDP 47011/47012/5353). Boots to tty1 with the pairing code, about 10 s after
the boot menu in QEMU. The code is printed by the agent's service onto the
console, next to the autologin prompt, until the tty1 UI exists.
`nixosModules.installer` works in any custom live image.
`services.nix-pixie-agent.relay.{enable,urls}` chooses the relays (§1); with
`openFirewall = false` and a relay in reach, the image opens no ports at
all.

**Stock live systems** (`packages.<system>.agent`): where only the
provider's NixOS ISO boots (Hetzner Cloud, say), the operator runs
`sudo nix --extra-experimental-features 'nix-command flakes' run
github:oddlama/nix-pixie#agent` on its console. That's the agent as
`nixosModules.agent` would run it, set up by a script:

- refuses to run without root (bash's `$EUID`, not `id`: a bare PATH must
  not make the check pass);
- appends the system profile and then its own tools (util-linux, kmod,
  OpenSSH, nixos-install, nixos-facter) to PATH. The system's `nix`, `zpool`
  (matching its kernel module) and `modprobe` win, also when started with a
  bare PATH (`systemd-run`);
- opens UDP 47011 and 47012 with `nixos-firewall-tool`, which handles both
  firewall backends. Not mDNS: mdns-sd answers unicast queries from any
  address, with more bytes than it got (RFC 6762 §11 says to ignore
  off-link queries). On the internet that is a reflector. Probes find the
  agent on a LAN anyway.

The system must bring Nix and, for the respective layouts, ZFS and mdadm's
udev rules; the stock NixOS ISO has all of them.

The agent's flags pass through (`… run github:oddlama/nix-pixie#agent --
--relay URL`). It waits at the relays like the installer, so the code alone
finds it; the open ports then are what gives a controller behind NAT a
direct path to a server with a public address, without any hole punching.

- Extends `installation-cd-minimal` and `netboot-minimal` and adds:
  - the agent service, plus agent UI on tty1 (pairing code, status, logs)
  - getty on tty2+ for manual debugging
  - `nix-grpc-daemon` and `nixos-facter`

  disko itself isn't needed, because diskoScript comes from the controller.
- **Generic image** from our flake. The user's flake can build a **baked**
  image (controller key, match hints, extra modules).
- **PXE:** dnsmasq proxyDHCP + iPXE + HTTP. The controller writes the kernel
  command line with its key, so PXE implies baked mode. Caveats:
  - The netboot initrd holds the whole squashfs in RAM, so machines with less
    than about 2 GiB can't netboot. The ISO works.
  - After install, the controller stops answering that MAC so the machine
    doesn't loop back into the installer.
- **Secure Boot (Microsoft-signed NixOS expected around mid 2027):** a
  modified image can't carry Microsoft's signature, because the kernel command
  line and initrd change (`init=` differs). Plan:
  1. Keep the agent a **portable userspace program** that can run on the
     stock signed installer (or any live Linux). Example: a static binary plus
     payload on a second USB partition, started with one typed command.
     Secure Boot doesn't restrict userspace. This same portability is what the
     kexec and rescue-system path needs.
  2. Allow signing our image with user keys (shim/MOK, or lanzaboote keys).
  3. Lanzaboote key enrollment = wait mode (setup mode is a firmware UI step).
- **Version skew:** the protocol version is in DNS-SD TXT and in the handshake.
  The controller supports the current and previous protocol versions, so old
  USB sticks keep working.

## 8. Nix interface

### User-facing outputs

- `flakeModules.default`: the `pixie` option tree. It produces:
  - a `pixie` flake output: JSON-serialisable machine metadata plus attribute
    paths, which the controller evaluates
  - `packages.<system>.pixie` (controller wrapper with the flake baked in, so
    `nix run .#pixie` works)
  - `packages.<system>.pixie-installer-{iso,netboot}` (baked images)
- `lib.mkPixie { self, machines, hooks, … }`: the **source of truth**. The
  flake-parts module is a thin wrapper, so the two can't drift apart.
- `nixosModules.installer`: the agent side.
- `nixosModules.default`: installed-system side (by-pixie udev rule, so only
  agnostic disks need it; later host keys).
- `lib.pinnedDisk file name`: pinned mode's device for disko disk `name`,
  falling back to `/dev/disk/by-pixie/<name>` until one is recorded, so the
  configuration builds before the first install.
- Our own `packages.<system>.{pixie, installer-iso, netboot}` work standalone:
  `nix run github:…/nix-pixie -- controller --flake .`.
- **Unknown output `pixie`:** `nix flake check` warns about it. That's
  harmless, but document it.
- **No flakes in `nix.conf` needed:** the controller runs the operator's `nix`
  with `--extra-experimental-features 'nix-command flakes'`. A repository
  that isn't a flake gets a small `flake.nix` that re-exports its
  configurations (README, "Without flakes"); its configurations must
  evaluate purely.

### Option sketch

```nix
pixie = {
  controller.trustedKeys = [ "<endpoint-id>" ];   # for baked images
  defaults = { hostKey.mode = "pull"; };
  machines.web1 = {
    configuration = "web1";                       # nixosConfigurations attr
    match = { mac = [ "aa:bb:cc:dd:ee:ff" ]; dmiSerial = [ ]; };  # hints only
    disks.file = "machines/web1/disks.json";      # pinned mode
    facter.reportPath = ./machines/web1/facter.json;
    hostKey = { mode = "push"; source = "pass show hosts/web1/ssh_host_ed25519_key"; };
    hooks.pre-build = [ /* derivations */ ];
    extraClosures = [ ];
  };
};
```

### Our repo layout

- **One flake, no partition.** Inputs: nixpkgs, flake-parts, nci, devshell,
  treefmt-nix, disko (non-flake, only for the checks; later nix-grpc-store).
  All flake inputs follow our nixpkgs.
  A dev partition would only keep devshell out of consumers' lockfiles: nci
  already depends on treefmt-nix, and Nix fetches unused inputs lazily. Not
  worth the extra flake. Revisit if consumers complain about lockfile size.
- **The consumer-facing module never imports our dev modules** (nci, devshell,
  treefmt). Using `flakeModules.default` adds options, not tooling.

```
flake.nix
rust-toolchain.toml
Cargo.toml                        # workspace
crates/nix-pixie                  # the one binary
crates/nix-pixie-proto            # wire protocol, shared with tests
nix/lib.nix                       # mkPixie: source of truth
nix/flake-module.nix              # flakeModules.default, thin wrapper over lib
nix/options.nix                   # option tree shared by both
nix/nixos/                        # agent, installer, installed-system modules
nix/tests/                        # eval checks and NixOS VM tests
docs/
```

### Tests

NixOS VM tests are the backbone. They cover:

- controller + target over a private VLAN without internet (airgapped)
- RAM-limited target with a closure bigger than its RAM
- each pairing mode, including a MITM relay attempt that must fail
- a target with no open ports on another network, found at a relay and
  installed through it, then again over a direct path with the host key
  check (done: `checks.vm-relay`, with nixpkgs' `iroh-relay` in a VM)
- pinned and agnostic disks (done: `checks.vm-disko`)
- each disk layout installed and booted from its own disks, with the host
  key check (done: `checks.vm-install-<layout>`, layouts in
  `nix/tests/layouts.nix`, BIOS GRUB)
- the shipped installer ISO booted with UEFI, installing systemd-boot, the
  firmware then booting the disk (done: `checks.vm-iso-uefi`); Secure Boot
  still to do
- pull-mode rekey (agenix-rekey fixture)
- interrupted push + resume

Running them: `checks [PATTERN]` in the devshell builds the matching checks
`JOBS` at a time (default 2). Evaluating a VM test takes about 1.4 GiB, and
`nix flake check` evaluates all of them in one process, which needs more
memory than a busy machine has; `checks` evaluates each on its own first.
A layout test takes 2 to 4 minutes and about 3 GiB while it runs; all
twenty-five checks take about 25 minutes with `JOBS=2`. The install tests fail
as soon as the install fails instead of waiting out their timeouts.

On a machine that is busy with other things, use `JOBS=1`. NixOS test VMs run
with `clocksource=acpi_pm`, a counter that wraps every 4.7 s. A VM that gets
no CPU for about that long (seen at a load of 28 on 16 cores) has a clock
that jumps backwards from then on: journald logs `Time jumped backwards`
every few seconds, noq warns `received a timestamp early than a previous
recorded time` thousands of times a second, and the transfer under way ends
with `connection lost: timed out`. That is the VM, not the install:
`vm-push` and `vm-install-ext4` failed like this once and passed unchanged
on a quiet machine.

## Future ideas

Not in v1. Recorded so the v1 design doesn't block them.

- **disko's own terminal.** Relaying it to the operator would make LUKS
  `askPassword` and ZFS `keylocation = "prompt"` work, which nix-pixie
  refuses today (keys come from files: hooks or the controller's
  terminal).

- **More of the relays.**
  - Controller codes through a relay: they would need a nameplate of their
    own, with the controller waiting at the rendezvous instead of the
    target.
  - The relays as an option of the `pixie` output, so a flake with its own
    relay doesn't need `--relay` on every command.
  - A relay of our own, as a NixOS module (nixpkgs has the `iroh-relay`
    package but no module), with an access token so it only serves its
    owner (`RelayMap::with_auth_token`).
  - Whether a displaced rendezvous (another endpoint connecting under the
    same id) comes back by itself is untested.
  - Checking the host key of a target behind NAT: only something on its
    network could, or the installed system dialing out.
- **Auto-approve.** Only for baked/PXE images (controller authenticated) and
  only for machines matching a pre-tag. Even then it stays per-machine opt-in,
  because a MAC/DMI match does not authenticate the hardware. Possibly gated
  on TPM endorsement-key attestation.
- **kexec / foreign live systems.** Run the portable agent on any live Linux
  (cloud rescue systems), bootstrapping `/nix` itself. Stock NixOS live
  systems are done (`packages.agent`).
