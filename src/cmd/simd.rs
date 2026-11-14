#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::io::{self};

pub(crate) const SIMD_THRESHOLD: usize = 4096;

/// Copies at least this large use non-temporal (streaming) stores.
pub(crate) const NT_THRESHOLD: usize = 1_048_576;

/// Writes sequential data across multiple extents with SIMD acceleration.
pub struct ExtentsWriter<'a, 'b> {
    extents: &'a mut [&'b mut [u8]],
    idx: usize,
    off: usize,
    simd: CpuSimd,
}
impl<'a, 'b> ExtentsWriter<'a, 'b> {
    /// Create a new ExtentsWriter for writing to the given extents.
    pub(crate) fn new(extents: &'a mut [&'b mut [u8]], simd: CpuSimd) -> Self {
        Self {
            extents,
            idx: 0,
            off: 0,
            simd,
        }
    }

    #[inline]
    fn current_extent_capacity(&self) -> usize {
        if self.idx < self.extents.len() {
            self.extents[self.idx].len().saturating_sub(self.off)
        } else {
            0
        }
    }

    /// Write data using optimized copying strategies with SIMD acceleration
    #[inline(always)]
    fn write_to_current_extent(&mut self, data: &[u8]) -> usize {
        let available = self.current_extent_capacity();
        if available == 0 || data.is_empty() {
            return 0;
        }
        let to_copy = available.min(data.len());

        // Bounds are guaranteed: current_extent_capacity() returned non-zero,
        // which ensures self.idx is valid and to_copy fits within the extent
        let extent = &mut self.extents[self.idx];
        let dest_slice = &mut extent[self.off..self.off + to_copy];
        let src_slice = &data[..to_copy];

        // Hot path first: large copies (>= SIMD_THRESHOLD) use SIMD — this is the common case
        if to_copy >= SIMD_THRESHOLD {
            simd_copy_large(self.simd, src_slice, dest_slice);
        } else {
            dest_slice.copy_from_slice(src_slice);
        }

        self.off += to_copy;
        if self.off >= extent.len() {
            self.idx += 1;
            self.off = 0;
        }
        to_copy
    }
}

impl<'a, 'b> io::Write for ExtentsWriter<'a, 'b> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let mut total_written = 0;

        while !buf.is_empty() {
            let written = self.write_to_current_extent(buf);
            if written == 0 {
                break; // no more capacity
            }

            total_written += written;
            buf = &buf[written..];
        }

        Ok(total_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Runtime CPU feature detection for SIMD acceleration.
// Cached via LazyLock; enable debug output with OTARIPPER_DEBUG_CPU=1.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub(crate) enum CpuSimd {
    None,
    Sse2,
    Avx2,
    Avx512,
}

#[cfg(target_arch = "x86_64")]
impl CpuSimd {
    fn detect() -> Self {
        let avx512f = is_x86_feature_detected!("avx512f");
        let avx512bw = is_x86_feature_detected!("avx512bw");
        let avx2 = is_x86_feature_detected!("avx2");
        let sse2 = is_x86_feature_detected!("sse2");

        let selected = if avx512f && avx512bw {
            CpuSimd::Avx512
        } else if avx2 {
            CpuSimd::Avx2
        } else if sse2 {
            CpuSimd::Sse2
        } else {
            CpuSimd::None
        };

        if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
            eprintln!("CPU Feature Detection:");
            eprintln!("  AVX512F: {}", avx512f);
            eprintln!("  AVX512BW: {}", avx512bw);
            eprintln!("  AVX2: {}", avx2);
            eprintln!("  SSE2: {}", sse2);
            eprintln!("  Selected: {:?}", selected);
        }

        selected
    }

    pub(crate) fn get() -> Self {
        use std::sync::LazyLock;
        static DETECTED: LazyLock<CpuSimd> = LazyLock::new(CpuSimd::detect);
        *DETECTED
    }
}

// For non-x86_64 targets, we use a simple fallback enum
#[cfg(not(target_arch = "x86_64"))]
#[derive(Debug, Clone, Copy)]
pub(crate) enum CpuSimd {
    None,
}

#[cfg(not(target_arch = "x86_64"))]
impl CpuSimd {
    pub(crate) fn get() -> Self {
        if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
            eprintln!("CPU Feature Detection: ARM64/Other architecture - using scalar operations");
        }
        CpuSimd::None
    }
}

/// SIMD-optimized large data copying
#[inline]
pub(crate) fn simd_copy_large(simd: CpuSimd, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    simd_copy_chunk(simd, src, dst);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn simd_copy_chunk(simd: CpuSimd, src: &[u8], dst: &mut [u8]) {
    match simd {
        CpuSimd::Avx512 => unsafe {
            if src.len() >= NT_THRESHOLD {
                simd_copy_avx512_stream(src, dst);
            } else {
                simd_copy_avx512(src, dst);
            }
        },
        CpuSimd::Avx2 => unsafe {
            if src.len() >= NT_THRESHOLD {
                simd_copy_avx2_stream(src, dst);
            } else {
                simd_copy_avx2(src, dst);
            }
        },
        CpuSimd::Sse2 => unsafe { simd_copy_sse2(src, dst) },
        CpuSimd::None => dst.copy_from_slice(src),
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn simd_copy_chunk(_simd: CpuSimd, src: &[u8], dst: &mut [u8]) {
    dst.copy_from_slice(src);
}

#[inline(always)]
pub(crate) fn is_all_zero_with_simd(simd: CpuSimd, data: &[u8]) -> bool {
    cfg_select! {
        target_arch = "x86_64" => {
            match simd {
                CpuSimd::Avx512 => unsafe { is_all_zero_avx512(data) },
                CpuSimd::Avx2 => unsafe { is_all_zero_avx2(data) },
                CpuSimd::Sse2 => unsafe { is_all_zero_sse2(data) },
                CpuSimd::None => data.iter().all(|&b| b == 0),
            }
        }
        _ => {
            // Non-x86 always scalar (auto-vectorized by LLVM)
            let _ = simd;
            data.iter().all(|&b| b == 0)
        }
    }
}

// === SIMD Copy Implementations ===
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
#[inline]
unsafe fn simd_copy_avx512(src: &[u8], dst: &mut [u8]) {
    let len = src.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(63);

    while i < simd_end {
        unsafe {
            let data = _mm512_loadu_si512(src_ptr.add(i) as *const __m512i);
            _mm512_storeu_si512(dst_ptr.add(i) as *mut __m512i, data);
        }
        i += 64;
    }

    if i < len {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn simd_copy_avx512_stream(src: &[u8], dst: &mut [u8]) {
    if src.len() < NT_THRESHOLD {
        unsafe {
            return simd_copy_avx512(src, dst);
        }
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();

    // vmovntdq faults on a destination that is not 64-byte aligned, so copy
    // the unaligned head with a regular copy first.
    let misalign = (dst_ptr as usize) & 63;
    let head = if misalign == 0 { 0 } else { 64 - misalign };
    if head > 0 {
        dst[..head].copy_from_slice(&src[..head]);
    }

    // Work in 64-byte blocks
    let mut i = head;
    let simd_end = head + ((src.len() - head) & !63);
    while i < simd_end {
        unsafe {
            let data = _mm512_loadu_si512(src_ptr.add(i) as *const __m512i);
            _mm512_stream_si512(dst_ptr.add(i) as *mut __m512i, data);
        }
        i += 64;
    }
    _mm_sfence(); // CRITICAL: Flushes non-temporal store buffers to RAM.
    // This ensures data is globally visible before we signal
    // that this operation is complete.

    // Tail
    if i < src.len() {
        dst[i..].copy_from_slice(&src[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn simd_copy_avx2(src: &[u8], dst: &mut [u8]) {
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = src.len().saturating_sub(31);

    while i < simd_end {
        unsafe {
            let data = _mm256_loadu_si256(src_ptr.add(i) as *const __m256i);
            _mm256_storeu_si256(dst_ptr.add(i) as *mut __m256i, data);
        }
        i += 32;
    }

    if i < src.len() {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn simd_copy_avx2_stream(src: &[u8], dst: &mut [u8]) {
    if src.len() < NT_THRESHOLD {
        unsafe {
            return simd_copy_avx2(src, dst);
        }
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();

    // vmovntdq faults on a destination that is not 32-byte aligned, so copy
    // the unaligned head with a regular copy first.
    let misalign = (dst_ptr as usize) & 31;
    let head = if misalign == 0 { 0 } else { 32 - misalign };
    if head > 0 {
        dst[..head].copy_from_slice(&src[..head]);
    }

    // Work in 32-byte blocks
    let mut i = head;
    let simd_end = head + ((src.len() - head) & !31);
    while i < simd_end {
        unsafe {
            let data = _mm256_loadu_si256(src_ptr.add(i) as *const __m256i);
            _mm256_stream_si256(dst_ptr.add(i) as *mut __m256i, data);
        }
        i += 32;
    }

    _mm_sfence(); // CRITICAL: Flushes non-temporal store buffers to RAM.
    // This ensures data is globally visible before we signal
    // that this operation is complete.
    // Tail
    if i < src.len() {
        dst[i..].copy_from_slice(&src[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
unsafe fn simd_copy_sse2(src: &[u8], dst: &mut [u8]) {
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = src.len().saturating_sub(15);

    while i < simd_end {
        unsafe {
            let data = _mm_loadu_si128(src_ptr.add(i) as *const __m128i);
            _mm_storeu_si128(dst_ptr.add(i) as *mut __m128i, data);
        }
        i += 16;
    }

    if i < src.len() {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
#[inline]
unsafe fn is_all_zero_avx512(data: &[u8]) -> bool {
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = data.len().saturating_sub(63);

    while i < simd_end {
        unsafe {
            let chunk = _mm512_loadu_si512(ptr.add(i) as *const __m512i);

            if _mm512_test_epi8_mask(chunk, chunk) != 0 {
                return false;
            }
        }
        i += 64;
    }
    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn is_all_zero_avx2(data: &[u8]) -> bool {
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = data.len().saturating_sub(31);

    while i < simd_end {
        unsafe {
            let chunk = _mm256_loadu_si256(ptr.add(i) as *const __m256i);

            if _mm256_testz_si256(chunk, chunk) == 0 {
                // ← Correct
                return false;
            }
        }
        i += 32;
    }
    data[i..].iter().all(|&b| b == 0)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
unsafe fn is_all_zero_sse2(data: &[u8]) -> bool {
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = data.len().saturating_sub(15);

    while i < simd_end {
        unsafe {
            let chunk = _mm_loadu_si128(ptr.add(i) as *const __m128i);
            let zero = _mm_setzero_si128();
            let cmp = _mm_cmpeq_epi8(chunk, zero);
            let mask = _mm_movemask_epi8(cmp);
            if mask != 0xFFFF {
                return false;
            }
            i += 16;
        }
    }
    data[i..].iter().all(|&b| b == 0)
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    /// Non-temporal stores (`vmovntdq`) fault on unaligned destinations.
    /// Decompressors return short reads, so `ExtentsWriter` can land a full
    /// 1 MiB write at any offset inside an extent. Sweep all 64 alignments
    /// for every SIMD level this CPU supports.
    #[test]
    fn large_copy_handles_any_dst_alignment() {
        let mut levels = vec![CpuSimd::None, CpuSimd::Sse2];
        if is_x86_feature_detected!("avx2") {
            levels.push(CpuSimd::Avx2);
        }
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            levels.push(CpuSimd::Avx512);
        }

        let len = NT_THRESHOLD + 37;
        let src: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut backing = vec![0u8; len + 64];
        for simd in levels {
            for misalign in 0..64 {
                backing.fill(0);
                let dst = &mut backing[misalign..misalign + len];
                simd_copy_large(simd, &src, dst);
                assert!(dst == &src[..], "{simd:?}: copy mismatch at misalign {misalign}");
            }
        }
    }
}
