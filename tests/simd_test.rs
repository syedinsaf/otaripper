#[test]
fn test_simd_implementation() {
    // Test with all zeros
    let zeros = vec![0u8; 1000];
    assert!(is_all_zero(&zeros));
    
    // Test with non-zeros
    let mut non_zeros = vec![0u8; 1000];
    non_zeros[500] = 1;
    assert!(!is_all_zero(&non_zeros));
    
    // Test small slices
    let small_zeros = vec![0u8; 16];
    assert!(is_all_zero(&small_zeros));
    
    let mut small_non_zeros = vec![0u8; 16];
    small_non_zeros[8] = 1;
    assert!(!is_all_zero(&small_non_zeros));
    
    // Test edge cases
    let empty: Vec<u8> = vec![];
    assert!(is_all_zero(&empty));
    
    let single_zero = vec![0u8];
    assert!(is_all_zero(&single_zero));
    
    let single_non_zero = vec![1u8];
    assert!(!is_all_zero(&single_non_zero));
}

// Copy the function from src/cmd.rs for testing
#[inline]
fn is_all_zero(data: &[u8]) -> bool {
    let len = data.len();

    // Fast path for small slices
    if len < 32 {
        return data.iter().all(|&b| b == 0);
    }

    // Check if we can use SIMD instructions
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { is_all_zero_avx2(data) };
        } else if is_x86_feature_detected!("sse2") {
            return unsafe { is_all_zero_sse2(data) };
        }
    }

    // Fallback to scalar implementation
    data.iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn is_all_zero_avx2(data: &[u8]) -> bool {
    use std::arch::x86_64::*;
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
#[inline]
unsafe fn is_all_zero_sse2(data: &[u8]) -> bool {
    use std::arch::x86_64::*;
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
