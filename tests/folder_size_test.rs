use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

#[test]
fn test_directory_size_calculation() {
    // Create a temporary directory for testing
    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();
    
    // Create some test files with known sizes
    let file1_path = test_dir.join("file1.txt");
    let file2_path = test_dir.join("file2.txt");
    let subdir_path = test_dir.join("subdir");
    
    // Create subdirectory
    fs::create_dir(&subdir_path).unwrap();
    
    // Create files with specific content
    let mut file1 = File::create(&file1_path).unwrap();
    file1.write_all(b"Hello, World!").unwrap(); // 13 bytes
    
    let mut file2 = File::create(&file2_path).unwrap();
    file2.write_all(b"Test content for file 2").unwrap(); // 23 bytes
    
    let file3_path = subdir_path.join("file3.txt");
    let mut file3 = File::create(&file3_path).unwrap();
    file3.write_all(b"Subdirectory file content").unwrap(); // 25 bytes
    
    // Calculate expected total size: 13 + 23 + 25 = 61 bytes
    let expected_size = 61u64;
    
    // Test the directory size calculation
    let actual_size = calculate_directory_size(test_dir).unwrap();
    
    assert_eq!(actual_size, expected_size, 
        "Expected directory size to be {} bytes, but got {} bytes", 
        expected_size, actual_size);
}

// Copy of the calculate_directory_size function from cmd.rs for testing
fn calculate_directory_size(path: &Path) -> std::io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let metadata = fs::metadata(path)?;

    if metadata.is_file() {
        return Ok(metadata.len());
    }

    if metadata.is_dir() {
        let mut total_size = 0u64;
        let entries = fs::read_dir(path)?;

        for entry in entries {
            let entry = entry?;
            let entry_path = entry.path();
            total_size += calculate_directory_size(&entry_path)?;
        }

        return Ok(total_size);
    }

    Ok(0)
}
