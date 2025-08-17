// Simple benchmark to demonstrate AVX512 vs AVX2 vs SSE2 performance
// Run with: rustc bench_avx512.rs && ./bench_avx512.exe

use std::arch::x86_64::*;
use std::time::Instant;

#[cfg(target_arch = "x86_64")]
unsafe fn is_all_zero_avx512(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Process 64 bytes at a time with AVX512
    let mut i = 0;
    let simd_end = len - 63;
    
    while i < simd_end {
        let chunk = _mm512_loadu_si512(ptr.add(i) as *const __m512i);
        let zero = _mm512_setzero_si512();
        let cmp = _mm512_cmpeq_epi8_mask(chunk, zero);
        
        // If any byte is non-zero, mask will not be all ones
        if cmp != 0xffff_ffff_ffff_ffff {
            return false;
        }
        i += 64;
    }
    
    // Handle remaining bytes
    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
unsafe fn is_all_zero_avx2(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Process 32 bytes at a time with AVX2
    let mut i = 0;
    let simd_end = len - 31;
    
    while i < simd_end {
        let chunk = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let zero = _mm256_setzero_si256();
        let cmp = _mm256_cmpeq_epi8(chunk, zero);
        let mask = _mm256_movemask_epi8(cmp);
        
        // If any byte is non-zero, mask will not be -1 (all bits set)
        if mask != -1 {
            return false;
        }
        i += 32;
    }
    
    // Handle remaining bytes
    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
unsafe fn is_all_zero_sse2(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Process 16 bytes at a time with SSE2
    let mut i = 0;
    let simd_end = len - 15;
    
    while i < simd_end {
        let chunk = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let zero = _mm_setzero_si128();
        let cmp = _mm_cmpeq_epi8(chunk, zero);
        let mask = _mm_movemask_epi8(cmp);
        
        // If any byte is non-zero, mask will not be -1 (all bits set)
        if mask != -1 {
            return false;
        }
        i += 16;
    }
    
    // Handle remaining bytes
    data[i..].iter().all(|&b| b == 0)
}

fn is_all_zero_scalar(data: &[u8]) -> bool {
    data.iter().all(|&b| b == 0)
}

fn benchmark(data: &[u8], name: &str, func: impl Fn(&[u8]) -> bool) {
    let iterations = 1000;
    let start = Instant::now();
    
    for _ in 0..iterations {
        func(data);
    }
    
    let elapsed = start.elapsed();
    let avg_ns = elapsed.as_nanos() / iterations as u128;
    
    println!("{:<15} | {:>8} ns | {:>8.2} MB/s", 
             name, 
             avg_ns,
             (data.len() as f64 * 1_000_000_000.0) / (avg_ns as f64 * 1_048_576.0));
}

fn main() {
    println!("AVX512 Performance Benchmark");
    println!("============================");
    println!("Data size: 1MB of zeros");
    println!();
    
    // Create 1MB of zeros
    let data = vec![0u8; 1024 * 1024];
    
    println!("{:<15} | {:>8} | {:>8}", "Implementation", "Time (ns)", "Throughput");
    println!("{:-<15} | {:-^8} | {:-^8}", "", "", "");
    
    // Test scalar implementation
    benchmark(&data, "Scalar", is_all_zero_scalar);
    
    #[cfg(target_arch = "x86_64")]
    {
        // Test SSE2 if available
        if is_x86_feature_detected!("sse2") {
            unsafe {
                benchmark(&data, "SSE2", |d| is_all_zero_sse2(d));
            }
        }
        
        // Test AVX2 if available
        if is_x86_feature_detected!("avx2") {
            unsafe {
                benchmark(&data, "AVX2", |d| is_all_zero_avx2(d));
            }
        }
        
        // Test AVX512 if available
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            unsafe {
                benchmark(&data, "AVX512", |d| is_all_zero_avx512(d));
            }
        }
    }
    
    println!();
    println!("CPU Features Detected:");
    #[cfg(target_arch = "x86_64")]
    {
        println!("SSE2:  {}", is_x86_feature_detected!("sse2"));
        println!("AVX2:  {}", is_x86_feature_detected!("avx2"));
        println!("AVX512: {}", is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw"));
    }
}
