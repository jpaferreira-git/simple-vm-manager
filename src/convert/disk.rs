//! Other hypervisors' disk files, read as the VM sees its disk: VMDK (VMware: sparse, stream-
//! optimized as in an .ova, flat, split in 2 GB pieces), VHD (Hyper-V's older, Virtual PC, Azure),
//! VHDX (Hyper-V), QCOW2 (Proxmox, KVM, QEMU; zlib or zstd compressed clusters too), VDI
//! (VirtualBox) and raw images. Each says which parts of the disk hold anything, so only those are
//! sent: `VhdStream` makes the dynamic VHD that XenServer's /import_raw_vdi takes in
//! (format=vhd), its 2 MiB blocks only where the disk has something.
//!
//! A disk's file is read through `ReadAt`: a file on this computer, a part of one (a disk inside an
//! .ova), or one on another machine (Proxmox, over SSH).

use std::collections::HashMap;
use std::io::{self, Read};

/// A VHD block: the unit sent. One is sent when any of it holds something.
pub const BLOCK: u64 = 2 << 20;

/// Random access to bytes.
pub trait ReadAt: Send {
    /// Fills `buf` from `off`; what's past the end reads as zeros.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn len(&self) -> u64;
}

/// A file on this computer.
pub struct FileAt {
    f: std::fs::File,
    len: u64,
}

impl FileAt {
    pub fn open(path: &std::path::Path) -> io::Result<FileAt> {
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        Ok(FileAt { f, len })
    }
}

impl ReadAt for FileAt {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::io::Seek;
        buf.fill(0);
        if off >= self.len {
            return Ok(());
        }
        let n = ((self.len - off).min(buf.len() as u64)) as usize;
        self.f.seek(io::SeekFrom::Start(off))?;
        self.f.read_exact(&mut buf[..n])
    }
    fn len(&self) -> u64 {
        self.len
    }
}

/// A part of another: `len` bytes from `base` (a file inside an .ova's tar).
pub struct PartAt {
    pub inner: Box<dyn ReadAt>,
    pub base: u64,
    pub len: u64,
}

impl ReadAt for PartAt {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        if off >= self.len {
            return Ok(());
        }
        let n = ((self.len - off).min(buf.len() as u64)) as usize;
        self.inner.read_at(self.base + off, &mut buf[..n])
    }
    fn len(&self) -> u64 {
        self.len
    }
}

/// Bytes held in memory (tests).
#[cfg(test)]
pub struct MemAt(pub Vec<u8>);

#[cfg(test)]
impl ReadAt for MemAt {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        let len = self.0.len() as u64;
        if off < len {
            let n = ((len - off).min(buf.len() as u64)) as usize;
            buf[..n].copy_from_slice(&self.0[off as usize..off as usize + n]);
        }
        Ok(())
    }
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

/// A disk image, as the VM sees its disk.
pub trait Disk: Send {
    /// Its size (the VM's disk), in bytes.
    fn size(&self) -> u64;
    /// For each `BLOCK` of the disk, whether it may hold something (false: all zeros for sure).
    fn map(&mut self) -> io::Result<Vec<bool>>;
    /// The disk's bytes at `off`: zeros where nothing was ever written, and past its end.
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Its kind, in words ("VMDK, stream-optimized").
    fn kind(&self) -> String;
}

fn bad(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}

fn blocks(size: u64) -> usize {
    size.div_ceil(BLOCK) as usize
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn read_vec(r: &mut dyn ReadAt, off: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    r.read_at(off, &mut v)?;
    Ok(v)
}

/// Reads [off, off+buf.len()) of a disk made of units of `unit` bytes, each read by `one`
/// (unit index, offset in it, where to put it).
fn by_units(off: u64, buf: &mut [u8], unit: u64, mut one: impl FnMut(u64, usize, &mut [u8]) -> io::Result<()>) -> io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let at = off + done as u64;
        let i = at / unit;
        let within = (at % unit) as usize;
        let n = ((unit as usize - within).min(buf.len() - done)) as usize;
        one(i, within, &mut buf[done..done + n])?;
        done += n;
    }
    Ok(())
}

/// Which BLOCKs hold something, read and looked at (a raw image, a fixed VHD).
fn scan(d: &mut dyn Disk) -> io::Result<Vec<bool>> {
    let n = blocks(d.size());
    let mut out = vec![false; n];
    let mut buf = vec![0u8; BLOCK as usize];
    for (i, slot) in out.iter_mut().enumerate() {
        let off = i as u64 * BLOCK;
        let len = (d.size() - off).min(BLOCK) as usize;
        d.read(off, &mut buf[..len])?;
        *slot = buf[..len].iter().any(|&b| b != 0);
    }
    Ok(out)
}

/// Marks the BLOCKs a range of the disk touches.
fn mark(map: &mut [bool], from: u64, len: u64) {
    if len == 0 {
        return;
    }
    let (a, b) = ((from / BLOCK) as usize, ((from + len - 1) / BLOCK) as usize);
    for slot in map.iter_mut().take(b + 1).skip(a) {
        *slot = true;
    }
}

// ---------------------------------------------------------------- raw

pub struct Raw {
    r: Box<dyn ReadAt>,
    size: u64,
    what: &'static str,
}

impl Raw {
    pub fn new(r: Box<dyn ReadAt>) -> Raw {
        let size = r.len();
        Raw { r, size, what: "raw image" }
    }
}

impl Disk for Raw {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        scan(self)
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if off >= self.size {
            buf.fill(0);
            return Ok(());
        }
        let n = ((self.size - off).min(buf.len() as u64)) as usize;
        buf[n..].fill(0);
        self.r.read_at(off, &mut buf[..n])
    }
    fn kind(&self) -> String {
        self.what.into()
    }
}

// ---------------------------------------------------------------- VHD

struct VhdIn {
    r: Box<dyn ReadAt>,
    size: u64,
    /// Dynamic: each block's sector (u32::MAX: not there), the block's size, its bitmap's size.
    bat: Option<(Vec<u32>, u64, u64)>,
}

fn open_vhd(mut r: Box<dyn ReadAt>) -> io::Result<VhdIn> {
    let len = r.len();
    let mut footer = read_vec(r.as_mut(), len.saturating_sub(512), 512)?;
    if &footer[0..8] != b"conectix" {
        // an old one with a 511-byte footer
        footer = read_vec(r.as_mut(), len.saturating_sub(511), 512)?;
        if &footer[0..8] != b"conectix" {
            footer = read_vec(r.as_mut(), 0, 512)?;
            if &footer[0..8] != b"conectix" {
                return Err(bad("This VHD's footer is missing: the file isn't whole."));
            }
        }
    }
    let size = be64(&footer, 48);
    match be32(&footer, 60) {
        2 => Ok(VhdIn { r, size, bat: None }),
        3 => {
            let at = be64(&footer, 16);
            let h = read_vec(r.as_mut(), at, 1024)?;
            if &h[0..8] != b"cxsparse" {
                return Err(bad("This VHD's header is damaged."));
            }
            let table = be64(&h, 16);
            let entries = be32(&h, 28) as usize;
            let block = be32(&h, 32) as u64;
            if block == 0 || block % 512 != 0 {
                return Err(bad("This VHD's block size can't be right."));
            }
            let raw = read_vec(r.as_mut(), table, entries * 4)?;
            let bat = (0..entries).map(|i| be32(&raw, i * 4)).collect();
            let bitmap = (block / 512).div_ceil(8).div_ceil(512) * 512;
            Ok(VhdIn { r, size, bat: Some((bat, block, bitmap)) })
        }
        4 => Err(bad("This VHD is a differencing disk: it holds only the changes to another one. Merge it into its parent first (in Hyper-V: Edit Disk, Merge).")),
        t => Err(bad(format!("This VHD is of a kind not known ({}).", t))),
    }
}

impl Disk for VhdIn {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        match &self.bat {
            None => scan(self),
            Some((bat, block, _)) => {
                let mut map = vec![false; blocks(self.size)];
                for (i, &e) in bat.iter().enumerate() {
                    if e != u32::MAX {
                        mark(&mut map, i as u64 * block, *block);
                    }
                }
                map.truncate(blocks(self.size));
                Ok(map)
            }
        }
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let size = self.size;
        match &self.bat {
            None => {
                buf.fill(0);
                if off < size {
                    let n = ((size - off).min(buf.len() as u64)) as usize;
                    self.r.read_at(off, &mut buf[..n])?;
                }
                Ok(())
            }
            Some((bat, block, bitmap)) => {
                let (block, bitmap) = (*block, *bitmap);
                let r = &mut self.r;
                by_units(off, buf, block, |i, within, out| {
                    let e = bat.get(i as usize).copied().unwrap_or(u32::MAX);
                    if e == u32::MAX || i * block >= size {
                        out.fill(0);
                        Ok(())
                    } else {
                        r.read_at(e as u64 * 512 + bitmap + within as u64, out)
                    }
                })
            }
        }
    }
    fn kind(&self) -> String {
        if self.bat.is_some() { "VHD, dynamic" } else { "VHD, fixed" }.into()
    }
}

// ---------------------------------------------------------------- VHDX

const VHDX_BAT: [u8; 16] = [0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08];
const VHDX_META: [u8; 16] = [0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E];
const VHDX_FILE_PARAMS: [u8; 16] = [0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B];
const VHDX_DISK_SIZE: [u8; 16] = [0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8];
const VHDX_SECTOR: [u8; 16] = [0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F];

struct Vhdx {
    r: Box<dyn ReadAt>,
    size: u64,
    block: u64,
    /// Each payload block's BAT entry.
    bat: Vec<u64>,
}

fn open_vhdx(mut r: Box<dyn ReadAt>) -> io::Result<Vhdx> {
    // the newer of its two headers
    let mut best: Option<(u64, Vec<u8>)> = None;
    for at in [64 * 1024, 128 * 1024] {
        let h = read_vec(r.as_mut(), at, 4096)?;
        if &h[0..4] == b"head" {
            let seq = le64(&h, 8);
            if best.as_ref().map_or(true, |(s, _)| seq > *s) {
                best = Some((seq, h));
            }
        }
    }
    let (_, h) = best.ok_or_else(|| bad("This VHDX's headers are damaged."))?;
    if h[48..64].iter().any(|&b| b != 0) {
        return Err(bad("This VHDX wasn't closed cleanly (its log has changes not yet written). Open it once in Hyper-V, or attach and detach it in Windows' Disk Management, then try again."));
    }
    let rt = read_vec(r.as_mut(), 192 * 1024, 64 * 1024)?;
    if &rt[0..4] != b"regi" {
        return Err(bad("This VHDX's region table is damaged."));
    }
    let n = le32(&rt, 8) as usize;
    let (mut bat_at, mut bat_len, mut meta_at) = (None, 0u32, None);
    for i in 0..n.min(2047) {
        let e = &rt[16 + i * 32..16 + i * 32 + 32];
        if e[0..16] == VHDX_BAT {
            bat_at = Some(le64(e, 16));
            bat_len = le32(e, 24);
        } else if e[0..16] == VHDX_META {
            meta_at = Some(le64(e, 16));
        }
    }
    let (bat_at, meta_at) = (bat_at.ok_or_else(|| bad("This VHDX has no block table."))?, meta_at.ok_or_else(|| bad("This VHDX has no metadata."))?);
    let mt = read_vec(r.as_mut(), meta_at, 64 * 1024)?;
    if &mt[0..8] != b"metadata" {
        return Err(bad("This VHDX's metadata is damaged."));
    }
    let count = le16(&mt, 10) as usize;
    let (mut block, mut size, mut sector, mut parent) = (0u64, 0u64, 512u64, false);
    for i in 0..count.min(2047) {
        let e = &mt[32 + i * 32..32 + i * 32 + 32];
        let (off, len) = (le32(e, 16) as u64, le32(e, 20) as usize);
        let item = read_vec(r.as_mut(), meta_at + off, len.max(8))?;
        if e[0..16] == VHDX_FILE_PARAMS {
            block = le32(&item, 0) as u64;
            parent = le32(&item, 4) & 2 != 0;
        } else if e[0..16] == VHDX_DISK_SIZE {
            size = le64(&item, 0);
        } else if e[0..16] == VHDX_SECTOR {
            sector = le32(&item, 0) as u64;
        }
    }
    if parent {
        return Err(bad("This VHDX is a differencing disk: it holds only the changes to another one. Merge it into its parent first (in Hyper-V: Edit Disk, Merge)."));
    }
    if block == 0 || size == 0 {
        return Err(bad("This VHDX's metadata is incomplete."));
    }
    let ratio = ((1u64 << 23) * sector / block).max(1);
    let payload = size.div_ceil(block);
    let total = payload + (payload.saturating_sub(1)) / ratio + 1;
    let raw = read_vec(r.as_mut(), bat_at, (total * 8).min(bat_len.max(8) as u64) as usize)?;
    let entries = raw.len() / 8;
    let bat = (0..payload)
        .map(|k| {
            let i = (k + k / ratio) as usize;
            if i < entries {
                le64(&raw, i * 8)
            } else {
                0
            }
        })
        .collect();
    Ok(Vhdx { r, size, block, bat })
}

impl Disk for Vhdx {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        let mut map = vec![false; blocks(self.size)];
        for (k, &e) in self.bat.iter().enumerate() {
            if e & 7 >= 6 {
                mark(&mut map, k as u64 * self.block, self.block.min(self.size - k as u64 * self.block));
            }
        }
        Ok(map)
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let (bat, r, size) = (&self.bat, &mut self.r, self.size);
        by_units(off, buf, self.block, |k, within, out| {
            let e = bat.get(k as usize).copied().unwrap_or(0);
            if e & 7 < 6 || off >= size {
                out.fill(0);
                Ok(())
            } else {
                r.read_at(((e >> 20) << 20) + within as u64, out)
            }
        })
    }
    fn kind(&self) -> String {
        "VHDX".into()
    }
}

// ---------------------------------------------------------------- QCOW2

struct Qcow2 {
    r: Box<dyn ReadAt>,
    size: u64,
    bits: u32,
    l1: Vec<u64>,
    zstd: bool,
    l2: HashMap<u64, Vec<u64>>,
    /// The last compressed cluster opened: its L2 entry and what it holds.
    last: Option<(u64, Vec<u8>)>,
}

fn open_qcow2(mut r: Box<dyn ReadAt>) -> io::Result<Qcow2> {
    let h = read_vec(r.as_mut(), 0, 112)?;
    let version = be32(&h, 4);
    if !(2..=3).contains(&version) {
        return Err(bad(format!("This QCOW2 is version {}: not known.", version)));
    }
    if be64(&h, 8) != 0 {
        return Err(bad("This QCOW2 is built on another image (a backing file): it holds only the changes. Merge them first: qemu-img convert -O qcow2 it.qcow2 whole.qcow2."));
    }
    if be32(&h, 32) != 0 {
        return Err(bad("This QCOW2 is encrypted: decrypt it first (qemu-img convert)."));
    }
    let bits = be32(&h, 20);
    if !(9..=21).contains(&bits) {
        return Err(bad("This QCOW2's cluster size can't be right."));
    }
    let mut zstd = false;
    if version == 3 {
        let incompat = be64(&h, 72);
        if incompat & 2 != 0 {
            return Err(bad("This QCOW2 is marked corrupt: check it with qemu-img check -r all."));
        }
        if incompat & 4 != 0 {
            return Err(bad("This QCOW2 keeps its data in another file (an external data file)."));
        }
        if incompat & 16 != 0 {
            return Err(bad("This QCOW2 uses extended L2 entries (subclusters): convert it first (qemu-img convert -O qcow2)."));
        }
        if incompat & 8 != 0 && be32(&h, 100) > 104 {
            zstd = h[104] == 1;
        }
    }
    let size = be64(&h, 24);
    let l1_size = be32(&h, 36) as usize;
    let l1_at = be64(&h, 40);
    let raw = read_vec(r.as_mut(), l1_at, l1_size * 8)?;
    let l1 = (0..l1_size).map(|i| be64(&raw, i * 8) & 0x00ff_ffff_ffff_fe00).collect();
    Ok(Qcow2 { r, size, bits, l1, zstd, l2: HashMap::new(), last: None })
}

impl Qcow2 {
    fn cluster(&self) -> u64 {
        1 << self.bits
    }
    fn l2_table(&mut self, at: u64) -> io::Result<&Vec<u64>> {
        if !self.l2.contains_key(&at) {
            if self.l2.len() >= 64 {
                self.l2.clear();
            }
            let c = self.cluster() as usize;
            let raw = read_vec(self.r.as_mut(), at, c)?;
            let t = (0..raw.len() / 8).map(|i| be64(&raw, i * 8)).collect();
            self.l2.insert(at, t);
        }
        Ok(&self.l2[&at])
    }
    /// A cluster's L2 entry (0: never written).
    fn entry(&mut self, i: u64) -> io::Result<u64> {
        let per = self.cluster() / 8;
        let l1 = self.l1.get((i / per) as usize).copied().unwrap_or(0);
        if l1 == 0 {
            return Ok(0);
        }
        Ok(self.l2_table(l1)?.get((i % per) as usize).copied().unwrap_or(0))
    }
}

impl Disk for Qcow2 {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        let mut map = vec![false; blocks(self.size)];
        let c = self.cluster();
        let per = c / 8;
        for (t, &l1) in self.l1.clone().iter().enumerate() {
            if l1 == 0 {
                continue;
            }
            let table = self.l2_table(l1)?.clone();
            for (j, &e) in table.iter().enumerate() {
                let compressed = e & (1 << 62) != 0;
                let zero = !compressed && e & 1 != 0;
                let host = e & 0x00ff_ffff_ffff_fe00;
                if compressed || (!zero && host != 0) {
                    let at = (t as u64 * per + j as u64) * c;
                    if at < self.size {
                        mark(&mut map, at, c.min(self.size - at));
                    }
                }
            }
        }
        Ok(map)
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let c = self.cluster();
        let size = self.size;
        let mut done = 0usize;
        while done < buf.len() {
            let at = off + done as u64;
            let (i, within) = (at / c, (at % c) as usize);
            let n = ((c as usize - within).min(buf.len() - done)) as usize;
            let out = &mut buf[done..done + n];
            let e = if at >= size { 0 } else { self.entry(i)? };
            if e & (1 << 62) != 0 {
                if self.last.as_ref().map_or(true, |(k, _)| *k != e) {
                    let x = 62 - (self.bits - 8);
                    let host = e & ((1u64 << x) - 1);
                    let sectors = ((e >> x) & ((1u64 << (self.bits - 8)) - 1)) + 1;
                    let csize = (sectors * 512 - (host & 511)) as usize;
                    let raw = read_vec(self.r.as_mut(), host, csize)?;
                    let mut plain = vec![0u8; c as usize];
                    if self.zstd {
                        let mut d = ruzstd::StreamingDecoder::new(&raw[..]).map_err(|e| bad(format!("A compressed cluster can't be read: {}", e)))?;
                        read_fill(&mut d, &mut plain)?;
                    } else {
                        read_fill(&mut flate2::read::DeflateDecoder::new(&raw[..]), &mut plain)?;
                    }
                    self.last = Some((e, plain));
                }
                out.copy_from_slice(&self.last.as_ref().unwrap().1[within..within + n]);
            } else if e & 1 != 0 || e & 0x00ff_ffff_ffff_fe00 == 0 {
                out.fill(0);
            } else {
                self.r.read_at((e & 0x00ff_ffff_ffff_fe00) + within as u64, out)?;
            }
            done += n;
        }
        Ok(())
    }
    fn kind(&self) -> String {
        "QCOW2".into()
    }
}

/// Reads as much as fits (a decompressor that stops short leaves zeros).
fn read_fill(r: &mut dyn Read, out: &mut [u8]) -> io::Result<()> {
    let mut got = 0;
    while got < out.len() {
        match r.read(&mut out[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(bad(format!("A compressed part can't be read: {}", e))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- VDI

struct Vdi {
    r: Box<dyn ReadAt>,
    size: u64,
    block: u64,
    extra: u64,
    data: u64,
    map: Vec<u32>,
}

fn open_vdi(mut r: Box<dyn ReadAt>) -> io::Result<Vdi> {
    let h = read_vec(r.as_mut(), 0, 512)?;
    if le32(&h, 64) != 0xBEDA_107F {
        return Err(bad("Not a VirtualBox disk."));
    }
    match le32(&h, 76) {
        1 | 2 => {}
        3 | 4 => return Err(bad("This VDI holds only changes to another disk (a snapshot or differencing image). Clone it into a whole disk first (VBoxManage clonemedium).")),
        t => return Err(bad(format!("This VDI is of a kind not known ({}).", t))),
    }
    let (blocks_at, data) = (le32(&h, 340) as u64, le32(&h, 344) as u64);
    let size = le64(&h, 368);
    let (block, extra, count) = (le32(&h, 376) as u64, le32(&h, 380) as u64, le32(&h, 384) as usize);
    if block == 0 {
        return Err(bad("This VDI's block size can't be right."));
    }
    let raw = read_vec(r.as_mut(), blocks_at, count * 4)?;
    let map = (0..count).map(|i| le32(&raw, i * 4)).collect();
    Ok(Vdi { r, size, block, extra, data, map })
}

impl Disk for Vdi {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        let mut map = vec![false; blocks(self.size)];
        for (i, &e) in self.map.iter().enumerate() {
            let at = i as u64 * self.block;
            if e < 0xFFFF_FFFE && at < self.size {
                mark(&mut map, at, self.block.min(self.size - at));
            }
        }
        Ok(map)
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let (m, r, size) = (&self.map, &mut self.r, self.size);
        let (block, extra, data) = (self.block, self.extra, self.data);
        by_units(off, buf, block, |i, within, out| {
            let e = m.get(i as usize).copied().unwrap_or(u32::MAX);
            if e >= 0xFFFF_FFFE || i * block >= size {
                out.fill(0);
                Ok(())
            } else {
                r.read_at(data + e as u64 * (block + extra) + extra + within as u64, out)
            }
        })
    }
    fn kind(&self) -> String {
        "VDI".into()
    }
}

// ---------------------------------------------------------------- VMDK

/// A sparse extent (KDMV): monolithic sparse, a 2 GB piece of a split disk, or stream-optimized.
struct Sparse {
    r: Box<dyn ReadAt>,
    size: u64,
    grain: u64,
    per_gt: u64,
    gd: Vec<u32>,
    gts: HashMap<usize, Vec<u32>>,
    compressed: bool,
    last: Option<(u32, Vec<u8>)>,
}

fn open_sparse(mut r: Box<dyn ReadAt>) -> io::Result<Sparse> {
    let mut h = read_vec(r.as_mut(), 0, 512)?;
    if &h[0..4] != b"KDMV" {
        return Err(bad("Not a sparse VMDK."));
    }
    let flags = le32(&h, 8);
    if le64(&h, 56) == u64::MAX {
        // stream-optimized: the real header is the footer, before the end-of-stream marker
        let len = r.len();
        let f = read_vec(r.as_mut(), len.saturating_sub(1024), 512)?;
        if &f[0..4] != b"KDMV" {
            return Err(bad("This stream-optimized VMDK's footer is missing: the file isn't whole."));
        }
        h = f;
    }
    let size = le64(&h, 12) * 512;
    let grain = le64(&h, 20) * 512;
    let per_gt = le32(&h, 44) as u64;
    let gd_at = if le64(&h, 56) != 0 { le64(&h, 56) } else { le64(&h, 48) };
    if grain == 0 || per_gt == 0 {
        return Err(bad("This VMDK's header is damaged."));
    }
    let tables = (size.div_ceil(grain)).div_ceil(per_gt) as usize;
    let raw = read_vec(r.as_mut(), gd_at * 512, tables * 4)?;
    let gd = (0..tables).map(|i| le32(&raw, i * 4)).collect();
    let compressed = flags & 0x10000 != 0 && le16(&h, 77) == 1;
    Ok(Sparse { r, size, grain, per_gt, gd, gts: HashMap::new(), compressed, last: None })
}

impl Sparse {
    fn gt(&mut self, t: usize) -> io::Result<Option<&Vec<u32>>> {
        let at = self.gd.get(t).copied().unwrap_or(0);
        if at == 0 {
            return Ok(None);
        }
        if !self.gts.contains_key(&t) {
            let raw = read_vec(self.r.as_mut(), at as u64 * 512, self.per_gt as usize * 4)?;
            self.gts.insert(t, (0..self.per_gt as usize).map(|i| le32(&raw, i * 4)).collect());
        }
        Ok(self.gts.get(&t))
    }
    fn grain_at(&mut self, g: u64) -> io::Result<u32> {
        let per = self.per_gt;
        Ok(self.gt((g / per) as usize)?.and_then(|t| t.get((g % per) as usize).copied()).unwrap_or(0))
    }
}

impl Disk for Sparse {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        let mut map = vec![false; blocks(self.size)];
        for t in 0..self.gd.len() {
            let Some(table) = self.gt(t)?.cloned() else { continue };
            for (j, &e) in table.iter().enumerate() {
                let at = (t as u64 * self.per_gt + j as u64) * self.grain;
                if e > 1 && at < self.size {
                    mark(&mut map, at, self.grain.min(self.size - at));
                }
            }
        }
        Ok(map)
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let g = self.grain;
        let mut done = 0usize;
        while done < buf.len() {
            let at = off + done as u64;
            let (i, within) = (at / g, (at % g) as usize);
            let n = ((g as usize - within).min(buf.len() - done)) as usize;
            let e = if at >= self.size { 0 } else { self.grain_at(i)? };
            let out = &mut buf[done..done + n];
            if e <= 1 {
                out.fill(0);
            } else if self.compressed {
                if self.last.as_ref().map_or(true, |(k, _)| *k != e) {
                    // a marker: the grain's first sector (u64), the compressed size (u32), the data
                    let head = read_vec(self.r.as_mut(), e as u64 * 512, 12)?;
                    let csize = le32(&head, 8) as usize;
                    let raw = read_vec(self.r.as_mut(), e as u64 * 512 + 12, csize)?;
                    let mut plain = vec![0u8; g as usize];
                    read_fill(&mut flate2::read::ZlibDecoder::new(&raw[..]), &mut plain)?;
                    self.last = Some((e, plain));
                }
                out.copy_from_slice(&self.last.as_ref().unwrap().1[within..within + n]);
            } else {
                self.r.read_at(e as u64 * 512 + within as u64, out)?;
            }
            done += n;
        }
        Ok(())
    }
    fn kind(&self) -> String {
        if self.compressed { "VMDK, stream-optimized" } else { "VMDK, sparse" }.into()
    }
}

/// A disk made of pieces one after another (a VMDK split in 2 GB files, or flat with a descriptor).
struct Pieces {
    parts: Vec<(u64, u64, Option<Box<dyn Disk>>)>,
    size: u64,
    what: String,
}

impl Disk for Pieces {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> io::Result<Vec<bool>> {
        let mut map = vec![false; blocks(self.size)];
        for (start, len, part) in self.parts.iter_mut() {
            let Some(p) = part else { continue };
            // a piece's own blocks are at its start, which may not fall on a block's edge: each
            // of its blocks marks what it covers here
            let sub = p.map()?;
            for (i, &on) in sub.iter().enumerate() {
                if on {
                    let from = i as u64 * BLOCK;
                    if from < *len {
                        mark(&mut map, *start + from, BLOCK.min(*len - from));
                    }
                }
            }
        }
        Ok(map)
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        let end = off + buf.len() as u64;
        for (start, len, part) in self.parts.iter_mut() {
            let (a, b) = (off.max(*start), end.min(*start + *len));
            if a >= b {
                continue;
            }
            if let Some(p) = part {
                p.read(a - *start, &mut buf[(a - off) as usize..(b - off) as usize])?;
            }
        }
        Ok(())
    }
    fn kind(&self) -> String {
        self.what.clone()
    }
}

/// Opens a file named in a VMDK descriptor (next to it).
pub type Opener<'a> = &'a mut dyn FnMut(&str) -> io::Result<Box<dyn ReadAt>>;

fn open_descriptor(text: &str, open: Opener) -> io::Result<Pieces> {
    let mut parts = vec![];
    let mut at = 0u64;
    let mut create = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("parentCID") {
            let v = v.trim_start_matches(|c: char| c == '=' || c.is_whitespace()).trim_matches('"');
            if !v.eq_ignore_ascii_case("ffffffff") {
                return Err(bad("This VMDK is a snapshot: it holds only the changes to its parent disk. Delete the VM's snapshots first (in VMware: Delete All Snapshots), which merges them."));
            }
        }
        if let Some(v) = line.strip_prefix("createType") {
            create = v.trim_start_matches(|c: char| c == '=' || c.is_whitespace()).trim_matches('"').to_string();
        }
        let w: Vec<&str> = line.splitn(4, ' ').collect();
        if w.len() < 3 || !matches!(w[0], "RW" | "RDONLY" | "NOACCESS") {
            continue;
        }
        let len = w[1].parse::<u64>().map_err(|_| bad("This VMDK's descriptor is damaged."))? * 512;
        let rest = w.get(3).copied().unwrap_or("");
        let (file, offset) = match rest.find('"').and_then(|a| rest[a + 1..].find('"').map(|b| (a, a + 1 + b))) {
            Some((a, b)) => (&rest[a + 1..b], rest[b + 1..].trim().parse::<u64>().unwrap_or(0) * 512),
            None => ("", 0),
        };
        let part: Option<Box<dyn Disk>> = match w[2] {
            "FLAT" | "VMFS" | "VMFSRAW" => {
                let r = open(file)?;
                let rlen = r.len().saturating_sub(offset);
                Some(Box::new(Raw { r: Box::new(PartAt { inner: r, base: offset, len: rlen.min(len) }), size: len, what: "flat" }))
            }
            "SPARSE" => Some(Box::new(open_sparse(open(file)?)?)),
            "ZERO" => None,
            "VMFSSPARSE" | "SESPARSE" => return Err(bad("This VMDK is a snapshot (a delta disk): it holds only the changes to its parent. Delete the VM's snapshots first (in VMware: Delete All Snapshots).")),
            other => return Err(bad(format!("This VMDK has a piece of a kind not known ({}).", other))),
        };
        parts.push((at, len, part));
        at += len;
    }
    if parts.is_empty() {
        return Err(bad("This VMDK's descriptor names no data files."));
    }
    Ok(Pieces { parts, size: at, what: if create.is_empty() { "VMDK".into() } else { format!("VMDK, {}", create) } })
}

// ---------------------------------------------------------------- opening one

/// Opens a disk image by what its first bytes say. `open_sibling` opens a file next to it, for a
/// VMDK descriptor that names its data files (None: a lone file, as inside an .ova).
pub fn open(mut r: Box<dyn ReadAt>, open_sibling: Option<Opener>) -> io::Result<Box<dyn Disk>> {
    let head = read_vec(r.as_mut(), 0, 512)?;
    if &head[0..4] == b"KDMV" {
        return Ok(Box::new(open_sparse(r)?));
    }
    if &head[0..4] == b"QFI\xfb" {
        return Ok(Box::new(open_qcow2(r)?));
    }
    if &head[0..8] == b"vhdxfile" {
        return Ok(Box::new(open_vhdx(r)?));
    }
    if le32(&head, 64) == 0xBEDA_107F {
        return Ok(Box::new(open_vdi(r)?));
    }
    if head.starts_with(b"# Disk DescriptorFile") || head.starts_with(b"# Disk Descriptor") {
        let len = r.len().min(64 * 1024) as usize;
        let text = String::from_utf8_lossy(&read_vec(r.as_mut(), 0, len)?).trim_end_matches('\0').to_string();
        let Some(o) = open_sibling else {
            return Err(bad("This VMDK is only a descriptor: its data is in other files next to it."));
        };
        return Ok(Box::new(open_descriptor(&text, o)?));
    }
    // a VHD's footer is at its end (and copied at its start when dynamic)
    if &head[0..8] == b"conectix" {
        return Ok(Box::new(open_vhd(r)?));
    }
    let len = r.len();
    if len >= 512 {
        let tail = read_vec(r.as_mut(), len - 512, 512)?;
        if &tail[0..8] == b"conectix" {
            return Ok(Box::new(open_vhd(r)?));
        }
    }
    Ok(Box::new(Raw::new(r)))
}

/// Opens a disk file on this computer (a VMDK descriptor's pieces from its folder).
pub fn open_path(path: &std::path::Path) -> io::Result<Box<dyn Disk>> {
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let mut sibling = move |name: &str| -> io::Result<Box<dyn ReadAt>> {
        let p = dir.join(name);
        Ok(Box::new(FileAt::open(&p).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", p.display(), e)))?))
    };
    open(Box::new(FileAt::open(path)?), Some(&mut sibling))
}

// ---------------------------------------------------------------- the VHD that's sent

/// A dynamic VHD made on the fly from a disk: its footer's copy, its header, its block table,
/// each block that holds something (its sector bitmap, then its 2 MiB), and its footer. Its length
/// is known before the first byte, as the HTTP upload needs.
pub struct VhdStream {
    disk: Box<dyn Disk>,
    used: Vec<u64>,
    next: usize,
    footer: [u8; 512],
    prefix: Option<Vec<u8>>,
    pending: Vec<u8>,
    at: usize,
    done: bool,
    len: u64,
}

/// What a VHD's checksum is: the bytes added up, inverted.
fn checksum(b: &[u8]) -> u32 {
    !b.iter().fold(0u32, |s, &x| s.wrapping_add(x as u32))
}

/// The VHD footer's cylinders, heads and sectors per track (as its specification works it out).
fn geometry(size: u64) -> (u16, u8, u8) {
    let mut total = size / 512;
    if total > 65535 * 16 * 255 {
        total = 65535 * 16 * 255;
    }
    let (spt, heads, cth);
    if total >= 65535 * 16 * 63 {
        spt = 255u64;
        heads = 16u64;
        cth = total / spt;
    } else {
        let mut s = 17u64;
        let mut c = total / s;
        let mut h = (c + 1023) / 1024;
        if h < 4 {
            h = 4;
        }
        if c >= h * 1024 || h > 16 {
            s = 31;
            h = 16;
            c = total / s;
        }
        if c >= h * 1024 {
            s = 63;
            h = 16;
            c = total / s;
        }
        spt = s;
        heads = h;
        cth = c;
    }
    ((cth / heads).min(65535) as u16, heads as u8, spt as u8)
}

impl VhdStream {
    /// `map`: the disk's `Disk::map`.
    pub fn new(disk: Box<dyn Disk>, map: &[bool]) -> VhdStream {
        let size = disk.size();
        let entries = blocks(size);
        let used: Vec<u64> = map.iter().enumerate().filter(|(_, &on)| on).map(|(i, _)| i as u64).take(entries).collect();
        let bat_bytes = (entries as u64 * 4).div_ceil(512) * 512;
        let data_start = 512 + 1024 + bat_bytes;
        // its footer
        let mut f = [0u8; 512];
        f[0..8].copy_from_slice(b"conectix");
        f[8..12].copy_from_slice(&2u32.to_be_bytes());
        f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        f[16..24].copy_from_slice(&512u64.to_be_bytes());
        let since2000 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).saturating_sub(946_684_800);
        f[24..28].copy_from_slice(&(since2000 as u32).to_be_bytes());
        // made by "tap": XenServer's own (readers take the size field, not the geometry)
        f[28..32].copy_from_slice(b"tap\0");
        f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        f[36..40].copy_from_slice(b"Wi2k");
        f[40..48].copy_from_slice(&size.to_be_bytes());
        f[48..56].copy_from_slice(&size.to_be_bytes());
        let (c, h, s) = geometry(size);
        f[56..58].copy_from_slice(&c.to_be_bytes());
        f[58] = h;
        f[59] = s;
        f[60..64].copy_from_slice(&3u32.to_be_bytes());
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let id = (nanos ^ ((size as u128) << 64) ^ (used.len() as u128)).to_le_bytes();
        f[68..84].copy_from_slice(&id);
        let sum = checksum(&f);
        f[64..68].copy_from_slice(&sum.to_be_bytes());
        // its header
        let mut h = vec![0u8; 1024];
        h[0..8].copy_from_slice(b"cxsparse");
        h[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        h[16..24].copy_from_slice(&1536u64.to_be_bytes());
        h[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        h[28..32].copy_from_slice(&(entries as u32).to_be_bytes());
        h[32..36].copy_from_slice(&(BLOCK as u32).to_be_bytes());
        let sum = checksum(&h);
        h[36..40].copy_from_slice(&sum.to_be_bytes());
        // its block table: each block that's sent, at its place after the table
        let mut bat = vec![0xFFu8; bat_bytes as usize];
        for (k, &b) in used.iter().enumerate() {
            let sector = (data_start + k as u64 * (512 + BLOCK)) / 512;
            bat[b as usize * 4..b as usize * 4 + 4].copy_from_slice(&(sector as u32).to_be_bytes());
        }
        let mut prefix = Vec::with_capacity(data_start as usize);
        prefix.extend_from_slice(&f);
        prefix.extend_from_slice(&h);
        prefix.extend_from_slice(&bat);
        let len = data_start + used.len() as u64 * (512 + BLOCK) + 512;
        VhdStream { disk, used, next: 0, footer: f, prefix: Some(prefix), pending: vec![], at: 0, done: false, len }
    }
    /// How many bytes it is, start to end.
    pub fn len(&self) -> u64 {
        self.len
    }
    /// How many of the disk's bytes it carries (its blocks).
    pub fn carried(&self) -> u64 {
        self.used.len() as u64 * BLOCK
    }
}

impl Read for VhdStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.pending.len() {
            self.at = 0;
            self.pending.clear();
            if let Some(p) = self.prefix.take() {
                self.pending = p;
            } else if self.next < self.used.len() {
                let b = self.used[self.next];
                self.next += 1;
                let mut buf = vec![0u8; 512 + BLOCK as usize];
                buf[..512].fill(0xFF);
                let off = b * BLOCK;
                let n = (self.disk.size().saturating_sub(off)).min(BLOCK) as usize;
                self.disk.read(off, &mut buf[512..512 + n])?;
                self.pending = buf;
            } else if !self.done {
                self.done = true;
                self.pending = self.footer.to_vec();
            } else {
                return Ok(0);
            }
        }
        let n = out.len().min(self.pending.len() - self.at);
        out[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn qemu_img() -> bool {
        Command::new("qemu-img").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("svm-disk-tests-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A 40 MiB disk with something here and there (and an odd tail), mostly empty.
    fn sample(dir: &Path) -> (PathBuf, Vec<u8>) {
        let mut v = vec![0u8; 40 << 20];
        let mut x = 12345u32;
        for (start, len) in [(0usize, 4096usize), (3 << 20, 70_000), (17 << 20, 1 << 20), ((40 << 20) - 1000, 1000)] {
            for b in &mut v[start..start + len] {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                *b = (x >> 16) as u8;
            }
        }
        let p = dir.join("disk.raw");
        std::fs::write(&p, &v).unwrap();
        (p, v)
    }

    fn convert(src: &Path, dst: &Path, fmt: &str, opts: &[&str]) {
        let mut c = Command::new("qemu-img");
        c.args(["convert", "-q", "-f", "raw", "-O", fmt]);
        for o in opts {
            c.arg(o);
        }
        let ok = c.arg(src).arg(dst).status().unwrap().success();
        assert!(ok, "qemu-img convert to {} {:?} failed", fmt, opts);
    }

    fn read_all(d: &mut dyn Disk) -> Vec<u8> {
        let mut out = vec![0u8; d.size() as usize];
        // in odd-sized pieces, to cross every boundary
        let mut at = 0usize;
        let step = 1_000_003usize;
        while at < out.len() {
            let n = step.min(out.len() - at);
            d.read(at as u64, &mut out[at..at + n]).unwrap();
            at += n;
        }
        out
    }

    fn check_map(map: &[bool], want: &[u8]) {
        for (i, &on) in map.iter().enumerate() {
            let a = i * BLOCK as usize;
            let b = (a + BLOCK as usize).min(want.len());
            if want[a..b].iter().any(|&x| x != 0) {
                assert!(on, "block {} has data but isn't in the map", i);
            }
        }
    }

    #[test]
    fn reads_every_format_qemu_makes() {
        if !qemu_img() {
            eprintln!("qemu-img isn't installed: skipped");
            return;
        }
        let dir = scratch("formats");
        let (raw, want) = sample(&dir);
        let cases: Vec<(&str, &str, Vec<&str>)> = vec![
            ("sparse.vmdk", "vmdk", vec![]),
            ("stream.vmdk", "vmdk", vec!["-o", "subformat=streamOptimized"]),
            ("flat.vmdk", "vmdk", vec!["-o", "subformat=monolithicFlat"]),
            ("split.vmdk", "vmdk", vec!["-o", "subformat=twoGbMaxExtentSparse"]),
            ("dynamic.vhd", "vpc", vec![]),
            ("fixed.vhd", "vpc", vec!["-o", "subformat=fixed"]),
            ("disk.vhdx", "vhdx", vec![]),
            ("disk.qcow2", "qcow2", vec![]),
            ("v2.qcow2", "qcow2", vec!["-o", "compat=0.10"]),
            ("zlib.qcow2", "qcow2", vec!["-c"]),
            ("zstd.qcow2", "qcow2", vec!["-c", "-o", "compression_type=zstd"]),
            ("dynamic.vdi", "vdi", vec![]),
            ("static.vdi", "vdi", vec!["-o", "static=on"]),
        ];
        for (name, fmt, opts) in cases {
            let p = dir.join(name);
            convert(&raw, &p, fmt, &opts);
            let mut d = open_path(&p).unwrap_or_else(|e| panic!("{}: {}", name, e));
            assert!(d.size() >= want.len() as u64, "{}: size {}", name, d.size());
            let got = read_all(d.as_mut());
            assert!(got[..want.len()] == want[..], "{} ({}) reads differently", name, d.kind());
            assert!(got[want.len()..].iter().all(|&b| b == 0), "{}: its tail isn't zeros", name);
            check_map(&d.map().unwrap(), &want);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_vhd_made_is_the_same_disk() {
        if !qemu_img() {
            eprintln!("qemu-img isn't installed: skipped");
            return;
        }
        let dir = scratch("vhd-out");
        let (raw, want) = sample(&dir);
        let q = dir.join("in.qcow2");
        convert(&raw, &q, "qcow2", &[]);
        let mut d = open_path(&q).unwrap();
        let map = d.map().unwrap();
        // only the blocks with something are carried
        assert_eq!(map.iter().filter(|&&b| b).count(), 4);
        let mut s = VhdStream::new(d, &map);
        let len = s.len();
        let mut out = vec![];
        s.read_to_end(&mut out).unwrap();
        assert_eq!(out.len() as u64, len);
        let v = dir.join("out.vhd");
        std::fs::write(&v, &out).unwrap();
        let st = Command::new("qemu-img").args(["compare", "-q", "-f", "raw", "-F", "vpc"]).arg(&raw).arg(&v).status().unwrap();
        assert!(st.success(), "qemu-img compare says the VHD differs");
        // and it reads back the same with our own reader
        let mut back = open_path(&v).unwrap();
        assert_eq!(back.kind(), "VHD, dynamic");
        assert!(read_all(back.as_mut())[..want.len()] == want[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn geometry_as_the_spec_says() {
        // 40 MiB: 17 sectors a track, 5 heads
        assert_eq!(geometry(40 << 20), (963, 5, 17));
        // past 127 GiB: the largest geometry
        assert_eq!(geometry(200 << 30).1, 16);
        assert_eq!(geometry(200 << 30).2, 255);
    }

    #[test]
    fn raw_disks_are_scanned() {
        let mut v = vec![0u8; (5 * BLOCK) as usize + 100];
        v[(2 * BLOCK) as usize + 7] = 1;
        v[(5 * BLOCK) as usize + 50] = 9;
        let mut d = open(Box::new(MemAt(v)), None).unwrap();
        assert_eq!(d.map().unwrap(), vec![false, false, true, false, false, true]);
    }

    #[test]
    fn a_snapshot_vmdk_is_refused() {
        let text = "# Disk DescriptorFile\nversion=1\nCID=12345678\nparentCID=87654321\ncreateType=\"vmfsSparse\"\nRW 2048 VMFSSPARSE \"x-delta.vmdk\"\n";
        let mut none = |_: &str| -> io::Result<Box<dyn ReadAt>> { Err(io::Error::new(io::ErrorKind::NotFound, "none")) };
        let e = open(Box::new(MemAt(text.as_bytes().to_vec())), Some(&mut none)).err().unwrap();
        assert!(e.to_string().contains("snapshot"));
    }
}
