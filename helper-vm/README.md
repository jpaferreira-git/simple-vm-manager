# Helper VM

A small Linux VM that looks after your XenServer or XCP-ng pool from outside it, through the
pool's own API. **Nothing is installed on the servers.** It replaces the Server Helper add-on that
ran in dom0 (`helper/`). If that add-on is on your servers, its setup offers to remove it.

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

**Every hour**: compares each server's clock with the right time (NTP).

**On its schedule**
- **Backups** to an NFS share: the VMs you pick, as `.xva` files taken from a snapshot. It
  keeps the last few of each, plus the pool's database.
- A **weekly health review** of the pool, written by Claude (or a plain summary without it).
  It covers servers, storage, alerts, backups, unused disks and old snapshots.

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
   backups and Claude.

Run `setup.sh` again to update the helper; your answers stay. `sudo svm-helper setup` asks the
questions again.

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
| `svm-helper backup ["VM" …]` | the backups now, or just the VMs named |
| `svm-helper xapi 'expr'` | one Python expression on the pool's API (`x` = `session.xenapi`) |
| `svm-helper shell` | a Python shell on the pool's API |
| `sudo svm-helper trust` | pin the servers' certificates again, after you renew one |
| `sudo svm-helper setup` | the questions again |

## How it's kept safe

- It logs in to the pool with its password, over HTTPS. Setup pins the servers' certificates:
  if a certificate changes, it stops and tells you instead of trusting it.
- It never uses SSH, and nothing runs in the servers' dom0.
- The service runs as the unprivileged user `svm-helper`, in a systemd sandbox. It can write
  only to its own folder and to the backups share.
- While you sign in to Claude, setup serves that one page, at a random link, and stops when
  you're done. The token stays on the VM: the page only says whether it worked.
- The pool's password, and Claude's token or API key, are in `/etc/svm-helper/secrets.json`,
  readable only by root and the helper. Setup asks before it adds your user to the helper's group.
- Where alerts go is kept in the pool's `other_config` (`svm-helper`), as before. The helper's
  heartbeat goes in `svm-helper-vm`.

Free software under the MIT license, given as it is, with no warranty.
