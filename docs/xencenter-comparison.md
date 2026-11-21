# Simple VM Manager and XenCenter

What Simple VM Manager does next to XenCenter, the official Windows console for XenServer, as of
Simple VM Manager **1.27.0** (November 2026). Simple VM Manager covers what a small or home pool
uses most, and adds a few things XenCenter doesn't do; the big-pool features (high availability,
Workload Balancing, Active Directory…) are left to XenCenter. The XenCenter column follows the
XenCenter 2026.6.0 manual.

✅ it does · ❌ it doesn't · ¹ ² ³ see the [notes](#notes) at the end

Spotted a mistake? [Open an issue](https://github.com/jpaferreira-git/simple-vm-manager/issues).

## Where it runs

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| Linux | ✅ | ❌ |
| Windows | ✅¹ | ✅ |
| macOS | ✅¹ | ❌ |
| Light and dark themes | ✅ | ❌ |
| A demo server, to try it without a real one | ✅ | ❌ |
| Passwords kept safe | ✅ | ✅ |

## Pools

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| Connect to a pool (its other servers lead to the coordinator) | ✅ | ✅ |
| Overview: servers, VMs, load, capacity, license | ✅ | ✅ |
| Install updates, server by server | ✅ | ✅ |
| Change every server's root password | ✅ | ✅ |
| Server status report, for support | ✅ | ✅ |
| Create a pool; add or remove a server | ❌ | ✅ |
| Make another server the coordinator | ❌ | ✅ |
| High availability | ❌ | ✅ |
| Workload Balancing | ❌ | ✅ |
| Disaster recovery | ❌ | ✅ |
| Rolling pool upgrade, to a new major version | ❌ | ✅ |
| Apply a license | ❌ | ✅ |
| Active Directory and roles | ❌ | ✅ |
| Citrix Virtual Apps and Desktops | ❌ | ✅ |

## Servers

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| Restart, shut down | ✅ | ✅ |
| Power on (Wake-on-LAN) | ✅ | ✅ |
| Maintenance mode | ✅ | ✅ |
| Restart the toolstack | ✅ | ✅ |
| Back up and restore | ✅ | ✅ |
| Its text console | ✅ | ✅ |
| Turn SSH access on or off | ❌ | ✅ |
| IP address, DNS and hostname | ✅ | ✅ |
| Renew its certificate | ✅ | ✅ |
| Its USB devices, and the VM each is passed to | ✅ | ✅ |
| Install your own certificate | ❌ | ✅ |
| Send its logs to a syslog server | ❌ | ✅ |
| Multipathing | ❌ | ✅ |
| GPUs | ❌ | ✅ |
| The control domain's memory | ❌ | ✅ |
| Supplemental packs and driver disks | ❌ | ✅ |

## Virtual machines

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| New VM from an ISO | ✅ | ✅ |
| New VM from a template | ✅ | ✅ |
| New VM from a snapshot | ✅ | ✅ |
| New VM set up by cloud-init | ✅ | ❌ |
| Start, shut down, reboot, suspend, resume, force | ✅ | ✅ |
| Its console, with Ctrl+Alt+Del | ✅ | ✅ |
| Console in a window of its own | ❌ | ✅ |
| Remote Desktop to Windows VMs | ❌ | ✅ |
| CPUs, memory, boot order, start with its server | ✅ | ✅ |
| Memory as a range (dynamic memory) | ❌ | ✅ |
| Disks: add, grow, remove; the DVD drive | ✅ | ✅ |
| Attach a disk that already exists | ❌ | ✅ |
| Network cards: add, remove, another network | ✅ | ✅ |
| USB pass-through | ✅ | ✅ |
| Virtual TPM and Secure Boot | ✅ | ✅ |
| GPU and vGPU | ❌ | ✅ |
| Install the VM Tools | ❌ | ✅ |
| Copy | ✅ | ✅ |
| Move to another server or storage, live | ✅ | ✅ |
| Move to another pool | ❌ | ✅ |
| Make a template | ✅ | ✅ |
| Delete, with its disks | ✅ | ✅ |
| Started in order, with a wait between them, when the pool starts (XenCenter: only in a vApp, or after a high-availability failure) | ✅² | ❌ |
| vApps (VMs started together, in order, by hand) | ❌ | ✅ |
| VM groups (VMs kept on different servers) | ❌ | ✅ |

## Organizing

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| A tree of pools, servers, VMs and storage, with a search | ✅ | ✅ |
| Tags, folders and custom fields | ❌ | ✅ |
| Saved searches | ❌ | ✅ |

## Snapshots

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| Take one (with its memory too), revert, delete | ✅ | ✅ |
| A template or a file from a snapshot | ✅ | ✅ |
| Snapshot schedules | ✅ | ✅ |
| A schedule's missed time made up | ✅² | ❌ |

## Import and export

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| .xva files | ✅ | ✅ |
| OVF and OVA packages, disk images (VHD, VMDK) | ❌ | ✅ |
| VMs from VMware (Conversion Manager) | ❌ | ✅ |

## Storage

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| How full each storage is; the pool's default | ✅ | ✅ |
| New NFS storage | ✅ | ✅ |
| New SMB, iSCSI, Fibre Channel or GFS2 storage | ❌ | ✅ |
| ISO libraries (NFS or SMB) | ✅ | ✅ |
| Upload ISO files (from this computer or a link), delete them | ✅ | ❌ |
| Repair storage that isn't mounted | ✅ | ✅ |
| Mount it again by itself, once the NAS is back | ✅³ | ❌ |
| Reclaim freed space | ❌ | ✅ |
| Detach or destroy storage for VMs | ❌ | ✅ |

## Networks

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| Every network; a new one on a VLAN or private; MTU; delete | ✅ | ✅ |
| Bonds | ❌ | ✅ |
| SR-IOV | ❌ | ✅ |

## Watching

| | Simple VMM | XenCenter |
|---|:---:|:---:|
| Live load | ✅ | ✅ |
| Performance history, up to a year | ✅ | ✅ |
| Alerts, and dismissing them | ✅ | ✅ |
| Limits: CPU, network, disk, memory, storage | ✅ | ✅ |
| The pool emails its alerts | ✅ | ✅ |
| A storage-full alert on EXT, XFS, NFS or SMB storage (XenServer can't raise it there) | ✅³ | ❌ |
| Alerts on your phone: Telegram, ntfy, Discord, Home Assistant, webhooks | ✅² | ❌ |
| A log of what was done | ✅ | ✅ |
| SNMP and NRPE monitoring | ❌ | ✅ |

## Notes

1. Built from source (`cargo run --release`): only Linux has a ready-made download, and only
   Linux is tested.
2. With the optional [Server Helper](../helper/), set up from a pool's *Helper* tab.
3. While the app is open; always, with the Server Helper.
