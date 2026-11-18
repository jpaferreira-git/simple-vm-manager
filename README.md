# Simple VM Manager

A small, fast desktop console for **XenServer** and **XCP-ng** pools. Connect to your servers, see their
pools, hosts, virtual machines, storage and networks at a glance, use each VM's screen from its
console, and start, shut down, reboot, suspend or resume virtual machines — in a light or dark theme, on Linux, Windows or macOS.

It is an independent, simpler take on the classic Windows management console layout. It is not
affiliated with or endorsed by the makers of XenServer or XCP-ng.

| Light | Dark |
|---|---|
| ![Light theme](docs/screenshot-light-vm.png) | ![Dark theme](docs/screenshot-dark-vm.png) |
| ![Home page, dark](docs/screenshot-dark-home.png) | ![Folded sidebar](docs/screenshot-folded.png) |
| ![A VM's console](docs/screenshot-dark-console.png) | ![Connecting to a server](docs/screenshot-dark-connect.png) |
| ![An ISO library's files](docs/screenshot-dark-isos.png) | ![Uploading an ISO from a link](docs/screenshot-dark-upload.png) |
| ![Creating a VM](docs/screenshot-dark-new-vm.png) | ![A new VM's advanced options](docs/screenshot-dark-new-vm-advanced.png) |
| ![A VM's snapshots](docs/screenshot-dark-snapshots.png) | ![Taking a snapshot](docs/screenshot-dark-snapshot-take.png) |
| ![Your templates](docs/screenshot-dark-templates.png) | ![A new VM from a template](docs/screenshot-dark-new-vm-template.png) |
| ![A server's Overview](docs/screenshot-dark-host.png) | ![A pool's Overview](docs/screenshot-dark-pool.png) |
| ![A pool's updates](docs/screenshot-dark-updates.png) | ![Updating a pool](docs/screenshot-dark-updating.png) |
| ![Every ISO file of every pool](docs/screenshot-dark-iso-library.png) | ![Where your VMs' disks are](docs/screenshot-dark-vm-storage.png) |
| ![Adding NFS storage for VMs](docs/screenshot-dark-new-storage.png) | ![Moving a VM](docs/screenshot-dark-move-vm.png) |

## Features

- **Servers and pools** — connect to a server by name or IP with your login; pool members redirect to the
  pool coordinator automatically. Earlier servers are remembered. Tick *Remember password* to keep the
  password in your system's keychain (GNOME Keyring, KWallet or KeePassXC on Linux; Credential Manager
  on Windows; Keychain on macOS; never in the app's own files): the app then reconnects to that server
  each time it starts. Right-click a server to *Forget saved password*.
- **Disconnect** — at the end of a pool's (or a lone server's) tabs, or on its right-click menu;
  the app doesn't reconnect to it at the next start.
- **Resource tree** — *Infrastructure* view (pool → servers → their VMs and storage; shared storage and
  virtual networks in folders) or *Objects* view (grouped by type), with a search box. The sidebar **folds** into a slim icon strip.
  A dot on each icon says how it's doing: VMs by power state (grey, unknown, while the server
  they run on is off or not answering: nothing can be done to them then); servers green when on,
  red when off or not answering; shared storage and ISO libraries green when mounted, red when their server is
  down (a server's own local storage and removable drives have none). A server that goes
  down without shutting down (power cut, crash) turns red within about half a minute, though the
  pool itself only gives up on it after ten: the app knocks on each server's address as it looks.
- **Home overview** — servers, running VMs, memory and storage in use at a glance, each server's load
  and the latest activity.
- **A VM's Overview** — what matters most, at a glance and to change right there: its name and
  description (each with a pencil), its state in a pill (where it runs, for how long), its system
  and address (to copy); its load over the last ten minutes, live (CPU, memory, disk and network,
  each a little graph); and four cards: **CPUs and memory** (steppers, applied while it's off),
  **Startup** (starts with its server, the boot order of a BIOS VM), **Disks** (add one, grow one,
  remove one; the ISO in its DVD drive, or none) and **Network** (another network for a card, add
  or remove a card). What can be done while it runs can be; the rest says to shut it down first.
  In a narrow window the graphs go two by two and the cards one under another.
- **A server's Overview** — how it's doing (state, time up, address, role, version, CPUs, memory,
  VMs running), its load over the last ten minutes, live (CPU, memory, its VMs' disks, its network
  cards), and four cards: **Capacity** (its memory as a bar: what the server keeps, what its VMs
  have, what's free; the virtual CPUs given out against the real ones), **VMs on this server** (the
  busiest first, each with its CPU's last ten minutes, its CPU now and its memory; a click shows
  it), **Storage** (each one it uses, how full) and **Network** (each card: its traffic now, its
  network, address and speed, the management card marked, an unplugged one said so). Near a limit
  (memory almost all taken, CPUs busy, more virtual CPUs than real ones, storage nearly full) the
  card turns amber, then red, saying what that means.
- **A pool's Overview** — one line with its version and its license (green; amber when it runs out
  within 30 days or is a trial; red once it has run out), then its servers' load added up, its
  capacity, its servers (each with its CPU and memory), all its VMs (the busiest first) and its
  storage. When something is wrong (a server off or not answering, storage not mounted, the license
  running out, servers on different versions) its Details tab gets an orange dot and says what,
  under *Needs attention*.
- **Pool updates** — *Updates* on the toolbar, or the Updates item on a pool's Overview: green
  only when every server has every update, yellow when updates are waiting (none for security) or
  a server must still restart for ones installed, red when security updates are waiting (the
  toolbar's dot says the same). For XenServer 8 and later, from the pool's update channel: what's
  waiting (security first, each with its kind, date and what it asks of the server; a click opens
  its page), each server's share, and *Check now* to fetch the latest from the channel. *Install*
  goes server by server, the coordinator first: the pool checked for readiness, the server's VMs
  moved to the others, the updates installed, the server restarted when they need it (or only its
  toolstack) and back before the next. VMs that can't move (on a server's own storage, no room
  elsewhere…) are never shut down unless you choose so: the update then won't start, every server
  checked first, saying which VMs and why; or, if you choose, they're moved with their disks to
  another server and brought back after (slower: their disks are copied twice, but none stops), or
  shut down for their server's restart and started again after. Each step shows as it goes (how long it's been), with every
  call to the servers and their answers below it, as in a terminal (with Copy); a server's restart
  is waited for quietly (going down, then starting up again), and the end says plainly anything
  left to do by hand.
- **Details** — a VM's tabs are Overview, Snapshots, Console, Logs and Details: the less used
  things (tags, guest tools, virtualization, high availability, every disk and card with its
  position and MAC), click a value to copy it. Pools and servers have Overview, Storage,
  Networking, Logs and Details, and servers a Console.
- **Console** — see a running VM's screen and use it with your keyboard and mouse (the keyboard goes
  to the VM while the mouse is over its screen), always scaled to fit. A keyboard button next to
  the tabs sends Ctrl+Alt+Del and the combinations your desktop keeps for itself (the Windows key,
  Alt+Tab…). It reconnects by itself after the VM reboots. A server has a Console tab too: its own text console
  (the control domain's, as in XenCenter), where you log in as root.
- **ISO libraries** — *ISO Library* on the toolbar opens a page with every ISO file of every pool:
  each library as a card (where it is, how many files it has, or that it isn't mounted; a click
  opens it), then all the files by name, with their library when there's more than one, each with
  its description, *New VM* and the bin, as on a library's own page. Next to its tabs are *Upload*
  (asking into which library when there are several), *Rescan* (every library) and
  *New ISO Library*, which adds a Windows (SMB) or NFS share of installation
  discs to a pool: just the address (and a login for SMB) up front, protocol versions under
  *Advanced* (SMB 3.0 and NFSv3 by default). The server mounts the share right away; if that fails
  you get the reason in plain words, the equivalent `xe` command and the server's answer, ready to copy.
  An ISO library shows a disc in the tree and an *ISO Files* tab listing its discs, each with a
  description you can edit in place (kept by the pool, as XenCenter's) and a bin to delete it from
  the share (asks first; a disc in a running VM's DVD drive can't go, and shut-down VMs that have it
  get their drive emptied rather than removed), with *Upload*, *Rescan* (to pick up files copied to
  the share) and *Disconnect* (asks first; the pool stops using it, nothing is deleted). *Upload* takes an .iso from your computer, or straight from a link on the web: the
  link is checked as you paste it (file name and size), and its bytes go through the app into the
  library as they arrive, never saved on your computer. Either way you get progress, speed, time
  left and Cancel; the share must be writable by the account the library uses, and names keep only
  letters, digits, dots and hyphens (the ISO driver's rule). Dropping an .iso on the window uploads
  it too, except on Wayland, where the window toolkit doesn't receive dropped files yet.
  The same actions are on the library's right-click menu.
- **Storage that isn't mounted** — as when a server started while the NAS was off: the storage gets
  a "!" in the tree (red when it's mounted nowhere, orange when only some servers lack it), its
  General tab says where it's missing, and *Repair* (in place of *Rescan*) mounts it again there,
  the coordinator first, explaining in plain words if the share won't answer. If the pool has lost
  its settings for it altogether (XenCenter's *Detach*), *Repair* asks where it is and connects the
  same library again, its discs and their descriptions kept. The app also does this by itself,
  once, when it finds storage unmounted (Settings → Storage); Logs says how it went.
- **VM storage** — *VM Storage* on the toolbar lists every pool's storage for VMs' disks: its kind
  (NFS, local disk…), where it is (the NFS folder, or the server it's in), how full it is, and which
  one is the pool's default, where new VMs' disks go and what suspending and moving VMs use
  (*Make Default* on the others, under the mouse; the pool's Overview marks it too). A pool with no
  shared storage is told what that costs: a VM moves to another server only by copying its disks.
  *New VM Storage* adds a folder an NFS server shares: every server of the pool mounts it, so VMs
  move between servers without their disks being copied, and a pool update can keep them running.
  *Browse* asks the server which folders it shares; the new storage can be made the pool's default
  as it's made. If the address is one of the pool's own VMs (a TrueNAS VM, say) a caution says what
  that means: when its server restarts, every VM with a disk there stops. A failure is explained as
  for an ISO library (an export that doesn't let the servers in as root, a folder that isn't there),
  with the `xe` command and the server's answer.
- **New VM** — on the toolbar, on a pool's or server's right-click menu, or from an ISO in an ISO
  library: pick the ISO, a size (Small, Medium, Large or your own numbers), the server it runs on
  (or any), where the disk goes (the storage that server sees) and the network (or none). The rest is worked out: the system's template from the ISO's name
  (Windows 11 gets UEFI, Secure Boot and its virtual TPM; an ISO that isn't recognised gets
  "Other install media", plain BIOS), the name ("Ubuntu 24.04"), all CPUs in one socket. Advanced
  has the system, boot mode, CPU layout and a description. It's made the way XenCenter makes
  one, checked first against the storage's free space and the server's CPUs (a yellow note when
  the server hasn't the memory free to start it), and then started with its console open, ready
  to install. The ISO's library, and the disk's storage when it's on the network, are checked as
  they're picked: a NAS that's off doesn't fail, the server waits for it for ever, so a library
  that doesn't answer in 15 seconds shows in a red box ("Minisforum is offline", with Try again)
  and Create waits; they're checked once more when Create is clicked, before anything is made. While it's made, the dialog shows each call to the server and its answer, as in a
  terminal (with Copy), and how long an answer has been awaited; if the server turns something
  down, what was made is undone and its answer stays there. Once made, the dialog can be closed
  while the VM starts (which can take a while). An ISO the app doesn't recognise still gives a
  readable name ("fedora-coreos-44.20260928.3.1-live-iso.x86_64.iso" → "Fedora CoreOS 44").
- **Templates** — the copy button next to a VM's camera (or its right-click menu) has *Make
  Template…*: a template from the VM, which stays as it is, its disks copied in an instant on their
  own storage. A snapshot's row has it too (from a snapshot of the disks only), so a VM that's on
  needn't stop; otherwise the VM has to be off, as a copy of a running one is like one after a power
  cut (said when a snapshot was taken while it ran). The dialog reminds you to generalize it first:
  Windows' sysprep; on Linux an empty machine ID and new SSH host keys. Your templates (not the
  built-in ones) are in a *Templates* folder in the pool, each with its page, New VM and Delete.
  *New VM* then offers *Create from: An ISO | A template*: from a template, its size as it is or
  other CPUs and memory, its disks copied in an instant where it is or in full onto other storage,
  and its network cards as they are or one on another network.
- **Copy VM** — in the same menu: a VM just like one that's off, with its own disks (an instant copy
  where they are, or in full onto other storage) and new network addresses.
- **Delete VM** — the bin next to a VM's tabs, or on its right-click menu. It asks first, and its disks go too unless unticked (a disk another VM also uses stays);
  its snapshots go with it, and ISO files in its DVD drive aren't touched. A VM that's on is
  turned off first, the button saying so ("Shut down and delete").
- **Snapshots** — the camera next to a VM's power buttons (or on its right-click menu) takes one:
  its disks as they are now, named with the date and time unless you name it, and, for a running VM,
  its memory too if you tick it, so that reverting resumes it right where it was. The VM's
  *Snapshots* tab lists them, the newest first: when each was taken, what it holds, a description
  you can edit in place, and "Now" on the one the VM's current state comes from. *Revert* (the
  arrow at a row's end) asks first, offers to take a snapshot of now before it, turns the VM off if
  it's on, puts it back, and then leaves it as it was at the time: resumed, started again, or off.
  The bin deletes one, with its copies of the disks. In a narrow window the tab keeps Revert and
  Delete in view, dropping the other columns first.
- **Move a VM** — the Move button next to a VM's tabs, *Move…* on its right-click menu, or drag it
  onto a server in the tree: one dialog for the server it goes to (each with its free memory, or
  why it can't take it) and the storage of each of its disks (where it is when that server sees
  it, else that server's own, or the roomiest it sees). Running, it keeps running: moved live,
  its disks copied as it runs when they change storage (also on the same server); off, its disks
  are copied and swapped in, and it starts on that server from then on. Its DVD drive is emptied
  first when its disc's library isn't seen where it goes. Progress in Activities.
- **Shut down or restart a server** — the red power icon and the green restart icon at the end of a
  server's tabs. Each asks first, saying which VMs move to the pool's other servers and which are
  shut down (their disks on that server's own storage), and warns when it's the pool's coordinator.
  Then it goes the way the pool expects: no new VMs sent to it, those VMs shut down, the rest moved,
  then the server; if a step fails it takes VMs again. A restarting server turns green and ringed
  in the tree, and Activities says when it's back. A server that's on but not taking VMs (as one
  left disabled) says so on its Overview, with *Take VMs again*, and in its pool's Details.
- **VM power actions** — Start, Shut Down, Reboot, Suspend, Resume, Force Shut Down, Force Reboot,
  enabled only when the server allows them, with confirmations for the disruptive ones. They're
  all behind one button at the right end of a VM's tabs: the power symbol in the colour of the
  VM's state (a ring turning while an action runs) and an arrow, opening a menu of the actions with
  their coloured icons and shortcuts (on the name's line when the window is narrow). They're on
  its right-click menu too.
- **Keyboard shortcuts** — Ctrl+B start · Ctrl+E shut down · Ctrl+R reboot · Ctrl+Y suspend/resume ·
  F5 refresh.
- **Activities** — next to the gear: what's under way (a ring turning, with how many), each with its
  server, how long it's been and how far it is (for disks moved or copied, and uploads, how much
  of how much and how fast: "11.7 GB of 20 GB · 110 MB/s"), and the last few that ended. Above them, what's
  wrong right now (a red dot on the button): a connection lost, a server off or not answering (and
  the VMs whose state that makes unknown), storage that isn't mounted; a click opens it. A server
  that stops answering or comes back is noted there too, and Refresh (F5) says how it went,
  servers that don't answer included. Long lines take two. A click on one (or on a row of the Logs
  tab) shows it in full: the server, when, how it went, and every call made to the server with its
  answer or its error, as the server gave it (passwords hidden), ready to copy.
- **Tables** — a column's edge in the header can be dragged to make it wider or narrower; a double
  click gives it back its own width. Long jobs (copying
  disks, mounting a share, starting a VM…) run as tasks on the server that the app looks at every
  two seconds: if the server stops answering (a power cut), the job is given up as failed after
  100 seconds, instead of waited on for hours. What was being made then (a VM, a copy, a template)
  is noted, even across closing the app; once the server's back, anything left half made under its
  name is offered for deleting. A dialog whose job can't be cancelled says *Close*, not *Cancel*:
  closing it leaves the job going, under Activities. Closing the app doesn't stop a job that's one
  task on the server (copying a VM, a snapshot, moving a running VM): the server finishes it. Only
  when something would be left half done (the app takes it through several steps: a new VM, a
  shut-down VM's disks moved, a revert, a pool's update, a server shut down, a VM deleted, an
  upload) it asks first, with a yellow note for each saying what would be left.
- **Settings** — the gear at the far left of the toolbar: the theme (light, dark, or the same as
  your desktop), the size of everything (90–125%; Ctrl + and Ctrl - too), and the saved passwords,
  each with *Forget* (deleted from the keychain; the app stops connecting there as it starts), even
  for a server no longer in the tree. Your choices are remembered.
- **Updates** — as it starts, the app asks GitHub whether there's a newer release (Settings can turn
  this off, or *Check now*). If there is, it offers it: *Update* downloads it, puts it in place of
  the copy in `~/.local/bin` and opens it again; a copy the package manager installed (`/usr/bin`)
  says how to update it instead.
- **Demo server** — try everything without a real server (*Try the Demo Server* on the Home page).
- Live refresh every 15 s, automatic reconnect. A server that's off is given up on after 6 s
  (the window never waits for it); if it's the one the app logs in through, the pool's other
  servers are tried, and if they all point to a coordinator that's off, the app says so and how to
  make another server the coordinator.

## Install on Arch Linux / Omarchy

### Option 1 — as a system package (recommended)

```bash
sudo pacman -S --needed git base-devel rust
git clone https://github.com/jpaferreira-git/simple-vm-manager.git
cd simple-vm-manager
makepkg -si
```

This builds the app and installs it with pacman, including the launcher entry and icon. Open it with
**Super + Space** (Omarchy's app launcher) and type *Simple VM Manager*, or run `simple-vm-manager`.

Update later with `git pull && makepkg -si`. Remove with `sudo pacman -R simple-vm-manager`.

### Option 2 — just for your user, no root

```bash
sudo pacman -S --needed git rust pkgconf openssl
git clone https://github.com/jpaferreira-git/simple-vm-manager.git
cd simple-vm-manager
./install.sh            # ./install.sh --uninstall removes it
```

It installs to `~/.local/bin`, `~/.local/share/applications` and `~/.local/share/icons`.

### Option 3 — prebuilt download

Each tagged release has a `simple-vm-manager-linux-x86_64.tar.gz` (built automatically by GitHub
Actions). Unpack it and run `./install.sh` inside: it installs the ready-made program without
building anything. You can also just run `./simple-vm-manager` directly.

### Omarchy / Hyprland tips

- The window reports the app id `simple-vm-manager`, so Hyprland rules can target it, e.g. in
  `~/.config/hypr/hyprland.conf`:
  ```
  windowrulev2 = float, class:^(simple-vm-manager)$
  windowrulev2 = size 1180 740, class:^(simple-vm-manager)$
  ```
- *Same as the system* (right-click the theme button) follows your desktop's light/dark preference; on Omarchy's dark
  themes pick **Dark** if the system setting isn't detected.
- Graphics: it draws with Vulkan (via `vulkan-icd-loader` + your Mesa/NVIDIA driver) and falls back to
  OpenGL automatically.

## Other systems

- **Debian / Ubuntu**: `sudo apt install build-essential pkg-config libssl-dev libxkbcommon-dev libwayland-dev libx11-dev`,
  install Rust from <https://rustup.rs>, then `./install.sh`.
- **Fedora**: `sudo dnf install gcc pkgconf-pkg-config openssl-devel libxkbcommon-devel wayland-devel`, Rust, then `./install.sh`.
- **Windows / macOS**: install Rust from <https://rustup.rs> and run `cargo run --release`.

## Using it

1. Click **Connect** (or the *Connect to a Server* link on the Home page), type the server's name or IP
   address, your user name (usually `root`) and password, and press **Connect**.
2. The server appears in the sidebar. Expand it to see its hosts and virtual machines.
3. Select a VM and use the icons next to its tabs, or right-click it, to start, stop, reboot or suspend it.
   Its **Console** tab shows the VM's screen.
4. Fold the sidebar with the « button next to the search box (the folded strip's top button opens
   it again); choose the theme under the gear (**Settings**).

Settings live in `~/.config/SimpleVMManager/` (Linux/macOS) or `%APPDATA%\SimpleVMManager\` (Windows):
`servers.txt` (server names and user names you used) and `settings.txt` (theme, sidebar).

## How it talks to servers

JSON-RPC over HTTPS on port 443 (`https://<server>/jsonrpc`), logging in with
`session.login_with_password`. Server certificates are accepted without a prompt, so use it on
networks you trust. It re-reads the pool every 15 seconds and right after each action; when a
connection drops it retries after 5 seconds, then every 2 minutes.

## Building from source

```bash
cargo build --release          # binary: target/release/simple-vm-manager
cargo run --release -- --theme dark --sidebar folded
```

Command-line options: `--theme system|light|dark`, `--sidebar open|folded`, and
`--screenshot <file.png> [--show home|objects|vm]` (starts the demo server, saves a picture and exits —
used for automated checks).

Built with Rust, [egui/eframe](https://github.com/emilk/egui) (wgpu renderer with an OpenGL
fallback), `ureq` + `native-tls` for HTTPS.

*Windows note:* with the Rust **GNU** toolchain, a few Windows-only libraries are pinned in
`Cargo.lock`, `build.rs` adds `winlibs/libshlwapi.a`, and you may need a local `.cargo/config.toml`
with `rustflags = ["-C", "dlltool=<path to the toolchain's self-contained dlltool.exe>"]`. The MSVC
toolchain, Linux and macOS need none of this.

## License

MIT — see [LICENSE](LICENSE).

The app's typeface is [Inter](https://rsms.me/inter/) © The Inter Project Authors, bundled under the
SIL Open Font License 1.1 ([assets/fonts/LICENSE-Inter.txt](assets/fonts/LICENSE-Inter.txt)).
