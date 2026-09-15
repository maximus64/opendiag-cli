Prebuilt `opendiag` binaries. Each archive also contains the README and license;
vehicle and ECU definitions are compiled into the binary.

| Platform | Archive |
| --- | --- |
| Linux (x86_64, glibc 2.35+) | `x86_64-unknown-linux-gnu.tar.gz` |
| macOS (Apple silicon) | `aarch64-apple-darwin.tar.gz` |
| Windows (32-bit, for 32-bit J2534 drivers) | `i686-pc-windows-gnu.zip` |

Verify a download against its `.sha256` file. The macOS binary is unsigned, so
clear the quarantine flag after unpacking:

```sh
xattr -d com.apple.quarantine opendiag
```
