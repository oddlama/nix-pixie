[📦 Installation](#-installation) | [🧭 Usage](#-usage) | [🔐 Security](#-security)

## 🧚 nix-pixie

nix-pixie is like
[magic-wormhole](https://github.com/magic-wormhole/magic-wormhole) but for
setting up NixOS machines, and you can integrate it into your existing
configuration. You basically boot the ISO image from this repository, the new
machine shows a pairing code like `482913-acid-acorn-acre`. On any other
machine that has access to your config repo, you just execute the nix pixie
controller and by entering the code it will automatically setup the target
machine without any further interaction from your side.

It uses [iroh](https://github.com/n0-computer/iroh) and SPAKE2 to automatically
find the target machine, so it works without SSH access, and even behind CG-NAT
or a firewall. This can also automate all the things that traditionally require
multiple steps, like secret rekeying, secure boot key enrollment, disk
encryption password setup or anything else you need via hooks. Finally, it
ensures that the target system is directly pushed to the disk, so this also works
on target devices with low RAM.

It is agnostic to the way you normally deploy machines and is specifically
meant as a tool for initial deployments.

- 🥚 **Solves the chicken-egg problem** of the initial setup: secrets required for host requires rekeying or out of band provisioning, both of which can be done with nix-pixie automatically
- 🔢 **Single SPAKE2 code** to pair and setup a machine
- 🔍 **Finds the target** via [iroh](https://github.com/n0-computer/iroh) on your network, over a direct cable without DHCP, or on another network through a relay
- 🪝 **Automates multi-step setup** which can including formatting disks, transferring host keys, rekeying secrets, enrolling secure boot keys, running nixos-facter, ...
- 🚚 **Straight to ~~jail~~ disk**, never pushes a system closure to RAM so you can directly install the final system configuration
- ✈️ **Works airgapped**, as the target needs neither the internet nor a binary cache

## 📦 Installation

You need a flake with the `nixosConfigurations` you want to install, and each
of them must describe its disks with [disko](https://github.com/nix-community/disko).
nix-pixie then needs to know a few things about your machines, which you expose
as a `pixie` output.

1. Add nix-pixie as an input to your flake
   ```nix
   inputs.nix-pixie.url = "github:oddlama/nix-pixie";
   ```
2. Import the NixOS module in the configurations you want to install.
   It keeps the `/dev/disk/by-pixie` names from [Disks](#disks) working on the installed system.
   ```nix
   nixosConfigurations.web1 = nixpkgs.lib.nixosSystem {
     modules = [
       disko.nixosModules.disko
       nix-pixie.nixosModules.default
       ./hosts/web1
     ];
   };
   ```
3. Define your machines. Every option is optional, a machine named `web1`
   installs `nixosConfigurations.web1` unless you say otherwise.
   ```nix
   pixie = nix-pixie.lib.mkPixie {
     inherit (self) nixosConfigurations;
     config.machines.web1 = {
       disks.file = "hosts/web1/disks.json"; # see Disks below
       hostKey = {
         mode = "pull"; # see Host keys below
         publicKeyPath = "hosts/web1/host.pub";
       };
     };
   };
   ```
4. Build the installer and write it to a USB stick
   ```sh
   nix build github:oddlama/nix-pixie#installer-iso
   sudo cp result/iso/*.iso /dev/sdX # your USB stick
   ```
5. Setup hooks to automate your setup flow. See [Hooks](#hooks).

There is no binary cache yet, so the first `nix build` and `nix run` compile
nix-pixie, which takes a few minutes. `nix flake check` will warn you about the
unknown output `pixie`, which is harmless.

<details>
<summary>Example with flake-parts</summary>

The flake-parts module has the same options and creates the `pixie` output for
you:

```nix
{
  inputs.flake-parts.url = "github:hercules-ci/flake-parts";
  inputs.nix-pixie.url = "github:oddlama/nix-pixie";
  # ...
  outputs = inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.nix-pixie.flakeModules.default ];
      pixie.machines.web1 = {
        disks.file = "hosts/web1/disks.json";
        hostKey.mode = "pull";
        hostKey.publicKeyPath = "hosts/web1/host.pub";
      };
    };
}
```

</details>

<details>
<summary>Using your own installer image</summary>

The ISO is the minimal NixOS installer plus `nixosModules.installer`, which
runs the agent and brings up wired networking. You can add that module to any
live system of your own, for example to set a relay or to add drivers:

```nix
packages.x86_64-linux.installer =
  (nixpkgs.lib.nixosSystem {
    modules = [
      "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
      nix-pixie.nixosModules.installer
      {
        nixpkgs.hostPlatform = "x86_64-linux";
        services.nix-pixie-agent.relay.urls = [ "https://relay.example.org" ];
      }
    ];
  }).config.system.build.isoImage;
```

</details>

## 🧭 Usage

Boot the target from the installer and plug it into your network. After a few
seconds it shows a code:

```
    nix-pixie pairing code:  482913-acid-acorn-acre
```

Now run the controller in your flake's repository, on your own machine and as
your normal user:

```console
$ nix run github:oddlama/nix-pixie -- controller install --machine web1 --code 482913-acid-acorn-acre
```

From here on the setup process should proceed automatically and leave you with
a full setup machine. The steps happening in between are:

1. **Pairing.** The controller looks for a target that shows this code, on your
   network and at a relay. Both sides then prove to each other that they know
   the code, without ever sending it. A wrong code gives an attacker nothing to
   work with, see [Security](#-security).
2. **Approval.** The target sends its inventory and you decide whether that is
   the machine you meant. Up to here it has received nothing from you.
3. **Preparation.** The disks are chosen, the host key is generated, your hooks
   run and the system is built. All of this can fail without consequences.
4. **Erasing.** You see exactly which disks get erased, and what is on them,
   and confirm with a `y`.
5. **Installing.** disko formats, the system is copied into the new `/nix/store`
   and `nixos-install` sets up the boot loader.
6. **First boot.** The target reboots. If your configuration runs an SSH
   server, the controller asks it for its host key every 30 seconds
   (`--boot-check-interval`), or right away when you press Enter, and checks
   that it is the right one.

- **`--machine`** can be left out for machines with `match.mac` or
  `match.dmiSerial` set, which pre-select the machine for a target that has
  them. With it, your configuration and hooks are checked before pairing, so a
  mistake doesn't cost you the code.
- **For scripts:** the result is printed as one JSON object on stdout,
  everything else goes to stderr. `--yes` approves the target and formats empty
  disks, `--wipe` also erases disks that hold data, and `--then poweroff` or
  `--then stay` replace the reboot.
- **Output:** when stderr isn't a terminal, or with `TERM=dumb`, nothing is
  redrawn: each step is announced on a line of its own (`▸ Send the system`)
  and Nix prints its own output, which is what you want in a log file or CI.
  `NO_COLOR` turns off colours, and `RUST_LOG=nix_pixie=debug` shows more of
  what happens.
- **Just looking:** `nix-pixie controller pair --code …` only pairs and prints
  the inventory, which is handy to see what a machine has before you write its
  configuration.

### Finding the target

A target announces itself in several ways and the controller automatically finds it:

- **On the same network** through mDNS and broadcast probes. This works through
  the default NixOS firewall on your machine.
- **Over a direct cable** between the two machines, without any DHCP server,
  using IPv6 link-local addresses.
- **On another network**, through a relay. While the target shows a code it
  also waits at a relay server under a name derived from the first part of the
  code (`482913`), and that is where the controller asks for it. Neither side
  needs an open port or a public address for this. Once they know each other
  they try to punch a direct connection through NATs and firewalls. If that
  fails, the install goes through the relay, which works but is slow.

By default the relays are the public ones run by [n0](https://n0.computer), the
makers of [iroh](https://github.com/n0-computer/iroh), which nix-pixie uses for
its connections. They can't read anything that goes through them, but they do
see who talks to whom. You can use your own instead (nixpkgs has `iroh-relay`)
or none at all:

- **Your own relay:** `--relay https://relay.example.org` on the controller,
  and `services.nix-pixie-agent.relay.urls` in the installer image. Both sides
  need the same one, so the target shows the flag next to its code.
- **No relay:** `--no-relay` and `services.nix-pixie-agent.relay.enable = false`.
  The code gets shorter (`42-acid-acorn-acre`) and only finds targets on your
  network.
- **A known address:** `--target HOST` asks that host instead of searching,
  for targets you can reach but which aren't on your network. This uses no
  relay either and needs UDP ports 47011 and 47012 to get to the target.

<details>
<summary>A direct cable and NetworkManager</summary>

NetworkManager's default profile unfortunately gives up on a cable without DHCP after 45
seconds and takes the link-local address away again (in a loop). You man need to tell it
to stop doing that:

```sh
nmcli connection modify "Wired connection 1" ipv4.method disabled ipv6.method link-local
```

</details>

### Disks

The disks come from your configuration's `disko.devices.disk`. The `device` attribute decides
how that get mapped to nix-pixie:

| `device` | Mode | What happens |
| --- | --- | --- |
| `nix-pixie.lib.pinnedDisk ./disks.json "main"` | pinned | You choose a disk on the first install. nix-pixie records its `/dev/disk/by-id/…` path in the machine's `disks.file` and uses it from then on. |
| `"/dev/disk/by-pixie/main"` | agnostic | You choose a disk on every install, nothing is recorded. One configuration can install many machines. |
| anything else | fixed | Must exist on the target and be known ahead of time. |

```nix
# hosts/web1/default.nix, with `disks.file = "hosts/web1/disks.json"` from above
disko.devices.disk.main = {
  device = nix-pixie.lib.pinnedDisk ./disks.json "main";
  type = "disk";
  content = { /* ... */ };
};
```

If exactly one disk is free for the last open role, it is taken. Otherwise
nix-pixie asks, or you pass `--disk main=nvme0n1` (a kernel name, a
`/dev/disk/by-id` name or a serial number). Disks that are in use, like the
installer's own USB stick, are never offered.

nix-pixie only ever writes data files you declared (`disks.json`, the host key,
a facter report) to your repository and runs `git add` on them. It never
commits.

### Host keys

A machine's SSH host key usually is the identity its secrets are encrypted for,
which is a chicken-and-egg problem if the key only appears on first boot. So
nix-pixie can settle the key before anything is built:

- **`hostKey.mode = "pull"`:** the target generates the key right after you
  approved it. The public half is written to `hostKey.publicKeyPath` in your
  repository, the private half never leaves the target and is moved onto the
  installed system.
- **`"push"`:** a [hook](#hooks) provides the private key, for hosts whose key
  already exists somewhere.
- **`"none"`** (default): the installed system generates its key on first boot,
  like any NixOS does.

In all cases the key ends up where your configuration's
`services.openssh.hostKeys` says, so impermanence setups work. After the
reboot, the controller compares what the machine presents over SSH with what
was installed. With `none` it can only tell you the new key.

With [agenix-rekey](https://github.com/oddlama/agenix-rekey), this works by having
a hook that rekeys after the key was pulled and before the build:

```nix
# In the pixie config. Runs `agenix rekey --add-to-git` from your flake's
# `agenix-rekey` package.
hooks.rekey = nix-pixie.lib.hooks.agenixRekey { };

# In the host's configuration. Falls back to agenix-rekey's dummy key
# until the first install recorded the real one.
age.rekey.hostPubkey = nix-pixie.lib.agenixRekey.hostPubkey ./host.pub;
```

### Hooks

Hooks are programs from your flake which the controller runs on your machine,
in your repository, at a point of the install that you choose. They are how
secrets get in and how you do whatever nix-pixie doesn't know about.

```nix
pixie = nix-pixie.lib.mkPixie {
  inherit (self) nixosConfigurations;
  config = {
    # For every machine
    hooks.disk-key = {
      command = "disk-key"; # packages.<system>.disk-key of this flake
      before = [ "build" ];
    };
    # Just for this one
    machines.web1.hooks.notify = {
      command = "notify";
      after = [ "boot" ];
    };
  };
};
```

`after` and `before` refer to other hooks or to the steps of the install:
`approve`, `facter`, `host-key`, `disks`, `build`, `format`, `copy`, `install`,
`finish` and `boot`. A hook runs as early as that allows, with your terminal
attached, so it can ask for a YubiKey touch.

- **`$PIXIE_CONTEXT`** is a JSON file with what is known so far: the machine,
  the target's inventory, the pulled host key, the chosen disks, the built
  system.
- **`$PIXIE_OUT/files/<path>`** is copied to `<path>` on the installed system,
  owned by root and with the mode you gave it.
- **`$PIXIE_OUT/disk-keys/<path>`** is a key that disko will read at `<path>`
  on the target, see below.
- **`$PIXIE_OUT/host-key`** is the private host key in push mode.

All hooks are built once before any of them runs (with `--machine` even before
pairing), so a hook can't change what the others do. A hook that fails aborts
the install.

### Encrypted disks

disko reads the keys for LUKS and ZFS from files on the target, and nix-pixie
puts them there. They come from a hook, and whatever no hook provided is asked
for on your terminal.

```nix
# The file must be under /tmp or /run, which the installer keeps in RAM
disko.devices.disk.main.content.partitions.root.content = {
  type = "luks";
  name = "cryptroot";
  passwordFile = "/tmp/secret.key";
  content = { /* ... */ };
};
```

```nix
packages.disk-key = pkgs.writeShellApplication {
  name = "disk-key";
  runtimeInputs = [ pkgs.jq pkgs.pass ];
  text = ''
    mkdir -p "$PIXIE_OUT/disk-keys/tmp"
    pass show "disks/$(jq -r .machine "$PIXIE_CONTEXT")" > "$PIXIE_OUT/disk-keys/tmp/secret.key"
  '';
};
```

The keys are sent over the encrypted channel, only ever written to RAM on the
target and deleted as soon as disko is done. Options that would make disko ask
on a terminal (`askPassword`, ZFS `keylocation = "prompt"`) are refused
before anything is erased, with a hint on what to use instead. For ZFS that is
to create the dataset with `keylocation = "file:///tmp/secret.key"` and to
switch it back with `postCreateHook = "zfs set keylocation=prompt <dataset>"`.

### Hardware reports

With `facter.reportPath = "hosts/web1/facter.json"` on a machine, the target
runs [nixos-facter](https://github.com/nix-community/nixos-facter) after you
approved it and the report is written to that path before the build, ready for
`hardware.facter.reportPath = ./facter.json`.

### Servers that only boot their own image

Some providers only let you boot their own ISOs, a stock NixOS installer if
you are lucky. For these there is the agent as a package, to be run on the
server's console:

```sh
sudo nix --extra-experimental-features 'nix-command flakes' run github:oddlama/nix-pixie#agent
```

It shows a code like the installer does, and the controller finds it through
the relay. If you'd rather not involve one, append `-- --no-relay` to the
command above and give the controller `--target <ip>`. That needs UDP ports
47011 and 47012 to reach the server. The agent opens them in the NixOS
firewall by itself, a firewall of your provider is yours to open.

<details>
<summary>Until there is a binary cache</summary>

The command above would compile nix-pixie in the live system's RAM right now,
which you don't want. Build it on your machine and copy it over instead. Yes,
this needs the one ssh login that nix-pixie otherwise saves you, sorry. On the
stock installer that means running `passwd` on the console first.

```sh
agent=$(nix build --no-link --print-out-paths github:oddlama/nix-pixie#agent)
nix copy -s --to ssh-ng://root@<ip> "$agent"
ssh root@<ip> systemd-run --unit=pixie-agent "$agent/bin/nix-pixie-agent"
ssh root@<ip> cat /run/nix-pixie/code
```

</details>

## 🔐 Security

The pairing code has two parts. The number is public and only says where to
meet. The three words are the secret: both sides use them as the password of a
[PAKE](https://en.wikipedia.org/wiki/Password-authenticated_key_agreement)
(SPAKE2) which is bound to the keys of their connection. Whoever doesn't know
the words can't complete it and gets exactly one guess per attempt, without
learning anything to try out offline. That is why three words are enough.

Protected:

- **Someone on your network** who listens or sits in the middle can't read or change anything, as everything is end-to-end encrypted and authenticated by the code
- **Guessing the code** gets three attempts before the target shows a new one, with pauses that grow up to a minute. That leaves about a one in 500,000 chance per day of trying.
- **A machine pretending to be your target** fails the pairing since it doesn't know the code, and the controller gives up after three of them
- **The relay** learns who talks to whom and how much, but none of the content. It can't pair with anything either.
- **Your own machine** opens no ports, and the target can't read your nix store, it only receives what you push
- **The wrong disk** is hard to hit: everything is built first, you confirm the exact disks and what is on them, scripts need `--wipe` for disks with data, disks in use are never offered and the target checks again right before formatting
- **Disk keys** never touch a nix store or a log, exist only in RAM on the target and are deleted as soon as the disks are formatted
- **The first SSH connection** to the new machine is no leap of faith with a pulled or pushed host key, since it was fixed before the build and is checked after the boot

Not protected:

- **Whoever sees the code before you use it** can pair with the target in your place, or pose as the target towards you. Treat it like a password until it is used.
- **A tampered installer image** controls everything the target does and sees every secret you send it. Build the image yourself if in doubt.
- **A target that lies about itself.** The inventory is what the machine claims to be, and MAC addresses or serial numbers that pre-select a machine are hints, not proof. This is why you approve it.
- **A target behind NAT** can't be asked for its host key after the reboot. nix-pixie tells you which key to expect instead of waiting.
- **`hostKey.mode = "none"`** leaves nothing to compare the first boot against
- **Your hooks** run with your rights and get the secrets they ask for, like everything else in your flake

Have a look at [`docs/design.md`](docs/design.md) for the details, the
protocol and the reasoning behind all of this.

## 🔨 TODO

There is still a lot I am going to add.

- A proper TUI for the controller and one for the target's screen
- A binary cache
- PXE boot, so the target doesn't even need a USB stick
- kexec into the installer from any running Linux
- Secure Boot, including enrolling lanzaboote keys during the install
- Resuming an interrupted install
- Running commands on the target from hooks (`systemd-cryptenroll` and friends)
- Installing a batch of machines in one go, with a single code that the controller shows

## ❤️ Contributing

Contributions are whole-heartedly welcome! Please feel free to suggest new
features, report the hardware nix-pixie didn't get along with, or generally
help out if you'd like. [`docs/design.md`](docs/design.md) explains how things
work and why, and is the place to read before changing behavior.

```sh
# Enter the devshell with all dependencies (or use direnv)
nix develop
# Format all files
nix fmt
# Run the unit tests and clippy
cargo test --workspace
cargo clippy --workspace --all-targets
# Run a single NixOS VM test, or all checks (takes about half an hour)
checks '^vm-relay$'
checks
```

`nix flake check` runs the same checks, but evaluates all VM tests in a single
process and needs a lot of memory for that. `checks` does them one by one.

## 🤖 LLM-ware

As you may have noticed, nix-pixie is LLM-ware. I designed it, but the
implementation was largely written by an LLM. It is tested by installing real
systems in VMs for every layout and scenario mentioned above, but at the end of
the day most of it is not written by a human.

## 📜 License

Licensed under the MIT license ([LICENSE](LICENSE) or <https://opensource.org/licenses/MIT>).
Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project by you, shall be licensed as above, without any additional terms or conditions.
