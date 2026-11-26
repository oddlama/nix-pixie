[📦 Installation](#-installation) | [🧭 Usage](#-usage) | [🔐 Security](#-security)

## 🧚 nix-pixie

nix-pixie is like
[magic-wormhole](https://github.com/magic-wormhole/magic-wormhole) but for
setting up NixOS machines, and you can integrate it into your existing
configuration. You basically boot the ISO image from this repository, the new
machine shows a pairing code like `482913-acid-acorn-acre`. On any other
machine that has access to your config repo, you just run the nix-pixie
controller with that code, approve the machine and its disks, and it sets up
the target machine without any further interaction from your side.

It finds the target machine by itself and connects to it with
[iroh](https://github.com/n0-computer/iroh), authenticated by the code through
SPAKE2, so it works without SSH access, and even behind CG-NAT or a firewall.
This can also automate all the things that traditionally require multiple
steps, like secret rekeying, host keys, disk encryption passwords, hardware
reports or anything else you need via hooks. Finally, it pushes the target
system directly to the disk, so this also works on target devices with low RAM.

It is agnostic to the way you normally deploy machines and is specifically
meant as a tool for initial deployments.

- 🥚 **Solves the chicken-and-egg problem** of the initial setup: a host's secrets need its host key, which requires rekeying after the first boot or provisioning the key out of band. nix-pixie does either before the one and only build
- 🔢 **Single SPAKE2 code** to pair and set up a machine
- 🔍 **Finds the target** by itself on your network, over a direct cable without DHCP, or on another network through an [iroh](https://github.com/n0-computer/iroh) relay
- 🪝 **Automates multi-step setup** which can include formatting disks, transferring host keys, rekeying secrets, running nixos-facter, ...
- 🚚 **Straight to ~~jail~~ disk**, never pushes a system closure to RAM so you can directly install the final system configuration
- ✈️ **Works airgapped**, as the target needs neither the internet nor a binary cache

## 📦 Installation

You need a flake with the `nixosConfigurations` you want to install (no flake?
see below), and each of them must describe its disks with
[disko](https://github.com/nix-community/disko). nix-pixie then needs to know a
few things about your machines, which you expose as a `pixie` output.

1. Add nix-pixie as an input to your flake
   ```nix
   inputs.nix-pixie.url = "github:oddlama/nix-pixie";
   ```
2. Describe each machine's disks with disko, as you would for any disko-based
   install. The simplest is to name the disk by its id:
   ```nix
   # hosts/web1/default.nix
   disko.devices.disk.main = {
     device = "/dev/disk/by-id/nvme-Samsung_SSD_990_PRO_1TB_S6Z1NJ0W123456";
     type = "disk";
     content = { /* your partitions */ };
   };
   ```
   Don't know the id yet? nix-pixie can let you pick the disk during the
   install instead, see [Disks](#disks).
3. Add the `pixie` output. Every configuration is a machine nix-pixie can
   install, and all options are optional: `defaults` set them for every
   machine (by its name), `machines.<name>` for one of them, winning over the
   defaults.
   ```nix
   pixie = nix-pixie.lib.mkPixie {
     inherit (self) nixosConfigurations;
     config = {
       # Generate each machine's SSH host key before the build, see Host keys below
       defaults = { name, ... }: {
         hostKey.mode = "pull";
         hostKey.publicKeyPath = "hosts/${name}/host.pub";
       };
       # Just for web1: pre-selects it for the target with this MAC address
       machines.web1.match.mac = [ "9c:6b:00:12:34:56" ];
       # A configuration that isn't for installing
       machines.my-iso.enable = false;
     };
   };
   ```
4. Get the installer and write it to a USB stick. Every
   [release](https://github.com/oddlama/nix-pixie/releases) has ISOs for
   x86_64 and aarch64 attached, built by GitHub Actions from the release's
   tag. Check that, or build the ISO yourself:
   ```sh
   gh attestation verify nix-pixie-installer-*.iso --repo oddlama/nix-pixie
   # or
   nix build github:oddlama/nix-pixie#installer-iso # result/iso/*.iso

   sudo cp nix-pixie-installer-*.iso /dev/sdX # your USB stick
   ```
5. Optionally, set up hooks to automate the rest of your flow, see [Hooks](#hooks).

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
      pixie.defaults = { name, ... }: {
        hostKey.mode = "pull";
        hostKey.publicKeyPath = "hosts/${name}/host.pub";
      };
    };
}
```

</details>

<details>
<summary>Without flakes</summary>

nix-pixie reads your machines from a flake, but your repository doesn't have
to be built around one. If you pin your dependencies with
[npins](https://github.com/andir/npins) or [niv](https://github.com/nmattia/niv),
a small `flake.nix` next to your existing entry point is enough, which hands
your configurations to nix-pixie:

```nix
{
  inputs.nix-pixie.url = "github:oddlama/nix-pixie";
  outputs = { self, nix-pixie }: {
    # Your existing configurations, e.g. `{ web1 = import ./hosts/web1 { ... }; }`,
    # given nix-pixie for its helpers
    nixosConfigurations = import ./configurations.nix { inherit nix-pixie; };
    pixie = nix-pixie.lib.mkPixie { inherit (self) nixosConfigurations; };
  };
}
```

A flake only sees files that git knows about, so `git add flake.nix`. Hooks
are packages of a flake, so expose them from this one if you use any. Your
configurations must evaluate without `<nixpkgs>` or `builtins.currentSystem`,
which npins and niv don't need since they pin with hashes. You don't have to
enable flakes in your `nix.conf` either: the controller enables what it needs
for its own calls to Nix, so this is all it takes to run it:

```sh
nix --extra-experimental-features 'nix-command flakes' run github:oddlama/nix-pixie -- controller install --machine web1 --code …
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

Leave out `--code` to type it in when it is needed, with Tab completing the
words. That also keeps the code out of your shell's history and the process
list.

From here on it runs by itself, apart from approving the machine and the disks
it erases, and leaves you with a fully set up machine. The steps happening in
between are:

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
- **Interruptions:** if the network goes away in the middle, say a cable comes
  loose or the laptop goes to sleep, both sides notice within 30 seconds. The
  controller then keeps trying to reach the target for 10 minutes, and the
  target waits as long. Once they are back in touch, the install goes on where
  it was: whatever the target was doing has finished meanwhile, and a copy that
  was cut off starts over, skipping what already arrived. If you stop the
  controller with Ctrl-C, the target shows a new code right away.
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
  fails, the install goes through the relay, which works but is slow. The
  target's screen says whether it reaches its relay, and when that changes.

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

NetworkManager's default profile waits for DHCP on a cable, gives up after 45
seconds and takes the link-local address away for a moment, again and again.
Each time, a running install can lose its connection and has to wait for it to
come back. Our installer prevents this by itself: on a
cable where nobody hands out addresses or advertises a router, it sends router
advertisements (as no router, and without any prefix), and that is all
NetworkManager needs to keep the link up.

With the agent package on another live system, tell NetworkManager on your
machine that link-local is all there is:

```sh
nmcli connection modify "Wired connection 1" ipv4.method disabled ipv6.method link-local
```

</details>

### Disks

nix-pixie formats the disks your configuration describes in
`disko.devices.disk`. Each disk's `device` says which of the target's disks it
is, in one of three ways:

| `device` | Mode | What happens |
| --- | --- | --- |
| `"/dev/disk/by-id/nvme-…"`, or any other path | fixed | The disk must exist on the target. Simple, but you need to know its id before the first install. |
| `nix-pixie.lib.pinnedDisk ./disks.json "main"` | pinned | You pick the disk during the first install, and nix-pixie writes its `/dev/disk/by-id/…` path into `disks.json` for you. Later installs use that disk. |
| `"/dev/disk/by-pixie/main"` | agnostic | You pick a disk on every install and nothing is recorded, so one configuration can install many machines. |

For a fixed disk, `nix-pixie controller pair --code …` tells you the ids: it
prints the target's inventory as JSON, with each disk's `by_id` names.

**Pinned** mode also needs to know where `disks.json` goes, relative to your
flake:

```nix
# In the pixie config
machines.web1.disks.file = "hosts/web1/disks.json";

# hosts/web1/default.nix
disko.devices.disk.main = {
  device = nix-pixie.lib.pinnedDisk ./disks.json "main";
  type = "disk";
  content = { /* ... */ };
};
```

You don't create `disks.json`. Until it exists, `pinnedDisk` returns a
placeholder, so your configuration evaluates and builds anyway. During the
first install, right after you picked the disk and before anything is built,
nix-pixie writes the file and `git add`s it, so the build already uses the
recorded disk:

```json
{
  "main": "/dev/disk/by-id/nvme-Samsung_SSD_990_PRO_1TB_S6Z1NJ0W123456"
}
```

It is JSON rather than Nix because nix-pixie writes it: plain data that
`pinnedDisk` reads with `lib.importJSON`, so nix-pixie never generates Nix code
in your repository. The facter report works the same way.

**Agnostic** mode also needs `nix-pixie.nixosModules.default` in the
configuration. During the install the chosen disk is linked at
`/dev/disk/by-pixie/main`, and some options keep using that path afterwards:
GRUB on a BIOS machine installs itself there on every rebuild. The module
recreates the link on the installed system, recognising the disk by the
partition labels disko gave it (so the disk needs a GPT). nix-pixie refuses an
agnostic configuration without the module before it erases anything. Fixed
and pinned disks don't need the module.

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

With [sops-nix](https://github.com/Mic92/sops-nix), the host decrypts its
secrets with an age key derived from its SSH host key, which is what sops-nix
does by default. A hook converts the pulled key, puts it into `.sops.yaml` and
re-encrypts the machine's secrets for it. Give the machine a line in
`.sops.yaml` with any age key (e.g. from `age-keygen`) until its first install:

```yaml
keys:
  - &admin age1...
  - &web1 age1... # replaced by the hook
creation_rules:
  - path_regex: hosts/web1/secrets\.yaml$
    key_groups:
      - age:
          - *admin
          - *web1
```

```nix
# In the pixie config
hooks.sops = {
  command = "sops-rekey";
  after = [ "host-key" ];
  before = [ "build" ];
};

# In your flake's packages. Adapt the paths to your layout.
packages.sops-rekey = pkgs.writeShellApplication {
  name = "sops-rekey";
  runtimeInputs = [ pkgs.jq pkgs.ssh-to-age pkgs.sops pkgs.gnused pkgs.git ];
  text = ''
    machine=$(jq -r .machine "$PIXIE_CONTEXT")
    key=$(jq -r .hostKey "$PIXIE_CONTEXT" | ssh-to-age)
    sed -i "s|^\( *- &$machine \).*|\1$key|" .sops.yaml
    sops updatekeys --yes "hosts/$machine/secrets.yaml"
    git add .sops.yaml "hosts/$machine/secrets.yaml"
  '';
};
```

`sops updatekeys` decrypts with your own key, so a YubiKey prompt works here
too.

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
- **`"$PIXIE" target exec -- PROGRAM ARGS...`** runs a program on the target,
  from `approve` until `finish`, with `--chroot` inside the installed system
  (from `install` on). Its output and exit code come back to your hook, as if
  it ran locally.

Programs on the target must be files in the Nix store, like
`${pkgs.sbctl}/bin/sbctl`. nix-pixie copies them there with their closure and
runs them without a shell, without input and with nothing of the installer's
environment (`PATH` leads nowhere). So a program behaves the same on every
target, and whatever it needs comes with it:

```nix
packages.on-target = pkgs.writeShellApplication {
  name = "on-target";
  text = ''
    # On the installer: the firmware's Secure Boot state
    "$PIXIE" target exec -- ${pkgs.sbctl}/bin/sbctl status
    # Inside the installed system (a hook after `install`)
    "$PIXIE" target exec --chroot -- ${pkgs.coreutils}/bin/install -d -m 700 /var/lib/backup
  '';
};
```

All hooks are built once before any of them runs (with `--machine` even before
pairing), so a hook can't change what the others do.

When a hook fails, say after a mistyped YubiKey PIN, you are asked whether to
run it again, with a fresh `$PIXIE_OUT`. The same goes for evaluating and
building your configuration, so you can fix a typo or a missing `git add` in
another terminal without pairing again. Only a fix that changes what the
install was planned for (the disks, the boot loader, the platform or the SSH
server) needs a fresh start. Without a terminal, the first failure aborts the
install.

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

### Secure Boot

nix-pixie installs systems that boot with
[lanzaboote](https://github.com/nix-community/lanzaboote). The easiest way lets
the machine make its own keys and enroll them on its first boot, so they never
leave it:

```nix
boot.lanzaboote = {
  enable = true;
  pkiBundle = "/var/lib/sbctl";
  autoGenerateKeys.enable = true;
  autoEnrollKeys = {
    enable = true;
    autoReboot = true;
  };
};
```

Keys can only be enrolled while the firmware is in Setup Mode, that is without
any Secure Boot keys. So before you boot the installer, clear them in the
firmware settings (look for "Reset to Setup Mode" or "Delete all keys"). That
also turns Secure Boot off, which the installer needs since it isn't signed.
The controller checks this before it erases anything. On its first boot the
machine creates its keys and reboots, systemd-boot enrolls them, and from then
on it boots with Secure Boot on. Microsoft's keys are enrolled as well,
because the firmware of some graphics and network cards is signed with them
(`autoEnrollKeys.includeMicrosoftKeys`). lanzaboote adds no boot entry to the
firmware: the machine starts whatever its disk boots by default, so that disk
has to come first in the boot order.

To bring your own keys instead, for example the same ones for a whole fleet,
leave out `autoGenerateKeys` and have a hook write them to
`$PIXIE_OUT/files/var/lib/sbctl/` before `install`.

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
- **A tampered installer image** controls everything the target does and sees every secret you send it. Verify a downloaded image with `gh attestation verify`, or build it yourself.
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
- Resuming an install after the controller itself was stopped
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
