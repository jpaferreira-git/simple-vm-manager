# Helper VM

A small Linux VM that looks after your XenServer or XCP-ng pool from outside it, through the
pool's own API. **Nothing is installed on the servers.** It replaces the Server Helper add-on that
once ran in dom0: if that add-on is still on your servers, its setup offers to remove it.

## What it does

**Every minute**
- Sends the pool's new alerts to **Telegram, ntfy, Discord, Home Assistant** or any webhook.
  Claude explains serious alerts in plain words (see [Claude](#claude) below).
- Checks that each server answers, and tells you when one stops answering, and when it's back.
- Tells you when it **can't reach the pool at all**, straight to your phone, because the pool
  can't send that alert itself.
- Watches the **services you name** (`http(s)://…` or `tcp://host:port`), such as Home
  Assistant or the NAS, and tells you when one is down.
- Raises an alert when a storage is fuller than its limit. This works for any kind of storage,
  including EXT, NFS and LVM, where XenServer's own alarm can't.
- Runs a snapshot schedule's missed run once (XenServer's plugin skips them).
- Mounts shared storage again on a server that started before the NAS was up.
- Starts the VMs on its list in order, once, after the pool starts.

**When Simple VM Manager converts VMs** (VMware or Proxmox, its Convert page): copies their
disks straight from the source into the pool, so they don't pass through your computer. It
reads them over HTTPS from ESXi or vCenter, and over SSH from Proxmox (the source's password
comes from the app for each copy, in a XenAPI secret, and isn't kept).

**Every hour**: compares each server's clock with the right time (NTP).

**Every day**: checks the pool against good practice for Simple VM Manager's **Advisor** page:
snapshot schedules, room on the storage, forgotten disks and old snapshots, the servers' clocks,
updates, memory and week's load, VM Tools, ISOs left in DVD drives, VMs that can't move to another
server, systems past their support, VMs off for months or at their limit all week, VMs that don't
start by themselves after a power cut, certificates, and whether alerts reach you. It reads each
server's and each VM's load over the week for it. Claude, given all of it, says why each thing
matters and what to do, and adds five to ten findings of its own. The app's *Check now* has it
check at once, and its *Send a test* has it send a test alert. On the Advisor you can dismiss a
check (it's left out from then on, and Claude doesn't bring it up again), or explain one to
Claude ("it's off on purpose: it's the spare"): it checks the pool again and goes by what you said.

**When the app asks**: it updates itself to the version that app comes with (the Advisor's and the
Helper tab's *Update it*).

**Every week**: a **health review** of the pool, written by Claude (or a plain summary without
it). It covers servers, storage, alerts, unused disks and old snapshots.

It also adds two things for you:
- The `svm-helper` command (see below).
- If you want it, **Claude Code**, with a `~/pool/CLAUDE.md` that tells it about this machine.
  With it, you can manage the pool by talking to it from the VM's terminal.

## Install

1. Make a small VM in the pool: Debian 12 (or Ubuntu 22.04+, Fedora, Rocky/Alma 9+, Arch),
   1 vCPU, 1 GB of memory and 8 GB of disk are plenty.
2. In its terminal:
   ```sh
   curl -fsSL https://raw.githubusercontent.com/jpaferreira-git/simple-vm-manager/main/helper-vm/setup.sh -o setup.sh
   less setup.sh      # read it first
   sudo bash setup.sh
   ```
3. Answer its questions: your pool (any server's address, the root user), where alerts go,
   and Claude.

Run `setup.sh` again to update the helper; your answers stay. From version 2.3 on, Simple VM
Manager can update it too: *Update it*, on the Advisor or the pool's Helper tab. `sudo svm-helper
setup` asks the questions again.

## Claude

Setup offers three ways for the helper to use Claude:

- **Your Claude subscription (Pro or Max), at no extra cost.** This is the recommended way.
  The helper installs Claude Code for its own user and runs it in print mode (`claude -p`, with
  no tools and no saved sessions). It logs in with a token that `claude setup-token` makes. To
  get one, setup shows a **QR code**. Your phone's camera opens a page on the VM (on your own
  network), with a button to sign in to Claude and approve, and a box for the code Claude then
  shows. Nothing has to be typed in the VM's console. Or run `claude setup-token` on any
  computer with Claude Code and paste its token. Its use counts in your plan's usage, like your
  own chats. It's small: a review a week, and at most 10 alert explanations a day.
- **An Anthropic API key**, billed per use on your Anthropic account.
- **Not at all.** The weekly review is then a plain summary.

## Commands

| Command | |
|---|---|
| `svm-helper status` | how it's doing, and what it did lately |
| `svm-helper check` | one look now |
| `svm-helper test` | a test alert to your channels |
| `svm-helper review [--send]` | this week's review, now |
| `svm-helper advise` | the Advisor's checklist, now (the app shows it too) |
| `svm-helper xapi 'expr'` | one Python expression on the pool's API (`x` = `session.xenapi`) |
| `svm-helper shell` | a Python shell on the pool's API |
| `sudo svm-helper trust` | pin the servers' certificates again, after you renew one |
| `sudo svm-helper setup` | the questions again |

## How it's kept safe

- It logs in to the pool with its password, over HTTPS. Setup pins the servers' certificates:
  if a certificate changes, it stops and tells you instead of trusting it.
- It never uses SSH to the pool's servers, and nothing runs in their dom0. (It uses SSH only to
  read a Proxmox VM's disks that you convert, while it copies them.)
- The service runs as the unprivileged user `svm-helper`, in a systemd sandbox. It can write
  only to its own folder.
- It can't install anything itself. When the app asks for an update, it leaves the version in its
  folder, and `svm-helper-update.service` (`update.sh`, as root) installs it: only a released
  version of this repository, through the same `setup.sh`.
- While you sign in to Claude, setup serves that one page, at a random link, and stops when
  you're done. The token stays on the VM: the page only says whether it worked.
- The pool's password, and Claude's token or API key, are in `/etc/svm-helper/secrets.json`,
  readable only by root and the helper. Setup asks before it adds your user to the helper's group.
- Where alerts go is kept in the pool's `other_config` (`svm-helper`), as before. The helper's
  heartbeat goes in `svm-helper-vm`, the Advisor's checklist in `svm-helper-advice`, what the
  app asks of it in `svm-helper-request`, and what you dismissed or explained on the Advisor in
  `svm-helper-notes`.

Free software under the MIT license, given as it is, with no warranty.
