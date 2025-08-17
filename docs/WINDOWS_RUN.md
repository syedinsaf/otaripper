Running otaripper on Windows

If otaripper is double-clicked without arguments, it will print an error and exit immediately.

Recommended ways to run:

1) From PowerShell or cmd.exe (recommended):

```powershell
& 'C:\path\to\otaripper.exe' -p 'C:\path\to\payload.zip'
```

2) Double-click wrapper: use the provided `run-otaripper.bat` in the project root. It runs the exe and pauses on error so you can read messages.

Troubleshooting if .\otaripper .\filename.zip does nothing for someone else:

- Ensure the recipient runs the command in a terminal, not by double-clicking the exe.
- If running from PowerShell, confirm they are using `.&` or `&` to start an executable (PowerShell requires `&` when path contains `.`): `& '.\otaripper' '.\filename.zip'`.
- Check Windows Defender / SmartScreen may block or silently terminate the process. Have them temporarily disable SmartScreen or run from an elevated PowerShell to see errors.
- Test on their machine by running `.	arget\release\otaripper.exe --help` to confirm the binary executes.
- If the binary was built on a different MSVC toolchain, ensure the recipient has matching VC++ redistributables installed (for non-static CRT builds).

If you want the program to show a GUI prompt or pause on exit automatically, I can add a small change to `src/main.rs` that detects no console and either shows a `MessageBoxW` or waits for a keypress. Let me know which behavior you prefer.
