# Folder Size Display Feature

## Overview

This feature automatically displays the total size of the extracted folder when the extraction process completes successfully. It provides users with immediate feedback about the extraction completion and helps them understand the storage requirements.

## What It Does

After successful extraction, the program now displays:
1. **Success confirmation**: "Extraction completed successfully!"
2. **Output directory path**: Shows where the files were extracted
3. **Total extracted size**: Displays the total size of all extracted files in human-readable format (e.g., "2.5 GB", "750 MB")
4. **Automatic folder opening**: Opens the extracted folder in your default file manager

## Implementation Details

### New Methods Added

#### `display_extracted_folder_size()`
- Called automatically after successful extraction
- Calculates and displays folder information
- Provides user-friendly output formatting

#### `calculate_directory_size()`
- Recursively calculates the total size of a directory and all its contents
- Handles both files and subdirectories
- Efficiently processes large directory structures
- Returns the total size in bytes

#### `open_extracted_folder()`
- Automatically opens the extracted folder in the default file manager
- Cross-platform support (Windows Explorer, macOS Finder, Linux file managers)
- Gracefully handles errors without interrupting the extraction process
- Can be disabled with the `--no-open-folder` flag

### Integration Points

- **Main extraction flow**: Called at the end of the `run()` method in `cmd.rs`
- **Error handling**: Only displays size information after successful completion
- **User experience**: Provides immediate feedback without requiring additional commands

## Example Output

```
Extraction completed successfully!
Output directory: C:\Users\Username\Desktop\extracted_20241201_143022
Total extracted size: 2.1 GB
```

## Benefits

1. **User confidence**: Immediate confirmation that extraction completed successfully
2. **Storage awareness**: Users know exactly how much space the extracted files occupy
3. **Verification**: Helps users verify that all expected data was extracted
4. **Convenience**: Automatically opens the folder so users can immediately access their extracted files
5. **No additional overhead**: Size calculation and folder opening happen automatically without performance impact

## Technical Features

- **Efficient calculation**: Uses recursive directory traversal optimized for performance
- **Human-readable output**: Sizes are displayed in appropriate units (B, KB, MB, GB, TB)
- **Error handling**: Gracefully handles permission issues or inaccessible files
- **Cross-platform**: Works on Windows, macOS, and Linux

## Testing

The feature includes comprehensive tests in `tests/folder_size_test.rs` that verify:
- Correct calculation of single file sizes
- Accurate directory size aggregation
- Proper handling of nested subdirectories
- Edge cases like empty directories

## Usage

No additional command-line options are required. The features activate automatically after successful extraction. Users will see the folder size information displayed and the folder will automatically open after the extraction progress bars complete.

### Disabling Folder Opening

If you prefer not to have the folder automatically open, use the `--no-open-folder` flag:

```bash
otaripper ota.zip --no-open-folder
```

## Future Enhancements

Potential improvements could include:
- Per-partition size breakdown
- Compression ratio information
- Estimated extraction time based on file sizes
- Storage space verification before extraction
