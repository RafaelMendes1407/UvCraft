# Linux launcher

Build the desktop application, then run the installer from the project root:

```sh
cargo build --release -p uvcraft-app
sh packaging/linux/install.sh
```

The installer copies the executable to `~/.local/bin`, the existing `icon.png` to the XDG pixmaps directory, and a desktop launcher to the XDG applications directory. Set `XDG_BIN_HOME` or `XDG_DATA_HOME` to use different per-user locations. The window icon is also embedded in the application binary.
