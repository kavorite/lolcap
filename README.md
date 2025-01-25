# lolcap

This thing watches you play League of Legends. Caveat emptor.

## Building

### Getting Rust

Install `rustup` for Windows from https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe. 


Then, run:
```batch
rustup toolchain install nightly
```

### The Netwide Assembler
This is required for a default feature in in [rav1e](https://github.com/xiph/rav1e/) for hardware-acceleration of video encoding on supported CPUs.

```batch
winget nasm
setx PATH "%PATH%;%LOCALAPPDATA%\bin\nasm"
```


> [!NOTE]
> If you don't have Windows 10 version 1809 or later, you may see
> `'winget' is not recognized as an internal or external command, operable program or batch file.`
> In that case, the recommended way to install it is through [its Microsoft Store page](https://apps.microsoft.com/detail/9nblggh4nns1?hl=en-US&gl=US).

### Compiling

```batch
cargo build --release
```

#### Optional: Configuring as a startup application

```batch
copy target\release\lolcap.exe "%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
```