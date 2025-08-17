use std::io::Write;
use std::time::Instant;
use sha2::{Digest, Sha256};
use otaripper::ExtentsWriter;
use memmap2::MmapMut;

fn make_backed_extents(file: &std::fs::File) -> MmapMut {
    // Safety: caller must ensure file length is set
    unsafe { MmapMut::map_mut(file).expect("failed to mmap temp file") }
}

fn copy_only(total: usize) {
    let tmp = tempfile::tempfile().expect("failed to create temp file");
    tmp.set_len(total as u64).expect("failed to set_len");
    let mut mmap = make_backed_extents(&tmp);

    // Single extent covering the entire mapping
    let mut slices: Vec<&mut [u8]> = vec![mmap.as_mut()];
    let mut writer = ExtentsWriter::new(slices.as_mut_slice());
    let src = vec![0xABu8; total];
    let start = Instant::now();
    let _ = writer.write(&src).unwrap();
    let dur = start.elapsed();
    let mb = (total as f64) / (1024.0 * 1024.0);
    println!("copy_only (file-backed): {:.3} s, {:.2} MB/s", dur.as_secs_f64(), mb / dur.as_secs_f64());
}

fn copy_then_hash(total: usize) {
    let tmp = tempfile::tempfile().expect("failed to create temp file");
    tmp.set_len(total as u64).expect("failed to set_len");
    let mut mmap = make_backed_extents(&tmp);

    let mut slices: Vec<&mut [u8]> = vec![mmap.as_mut()];
    let mut writer = ExtentsWriter::new(slices.as_mut_slice());
    let src = vec![0xABu8; total];
    let start = Instant::now();
    let _ = writer.write(&src).unwrap();

    // separate hash pass over the file-backed mapping
    let mut hasher = Sha256::new();
    hasher.update(&mmap[..]);
    let _ = hasher.finalize();

    let dur = start.elapsed();
    let mb = (total as f64) / (1024.0 * 1024.0);
    println!("copy_then_hash (file-backed): {:.3} s, {:.2} MB/s", dur.as_secs_f64(), mb / dur.as_secs_f64());
}

fn hash_while_write(total: usize) {
    let tmp = tempfile::tempfile().expect("failed to create temp file");
    tmp.set_len(total as u64).expect("failed to set_len");
    let mut mmap = make_backed_extents(&tmp);

    let mut slices: Vec<&mut [u8]> = vec![mmap.as_mut()];
    let mut writer = ExtentsWriter::new_with_hasher(slices.as_mut_slice());
    let src = vec![0xABu8; total];
    let start = Instant::now();
    let _ = writer.write(&src).unwrap();
    let _ = writer.finalize_hash();
    let dur = start.elapsed();
    let mb = (total as f64) / (1024.0 * 1024.0);
    println!("hash_while_write (file-backed): {:.3} s, {:.2} MB/s", dur.as_secs_f64(), mb / dur.as_secs_f64());
}

fn main() {
    // 10 GiB test file
    let total = 10usize * 1024 * 1024 * 1024; // 10 GiB

    println!("Running file-backed micro-benchmarks ({} MiB)", total / (1024*1024));
    copy_only(total);
    copy_then_hash(total);
    hash_while_write(total);
}
