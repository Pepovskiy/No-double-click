# faulty-mouse-debounce

A small Rust/Linux helper for Wayland systems such as Bazzite. It grabs a faulty physical mouse with `EVIOCGRAB`, filters accidental second left-clicks, and re-emits the cleaned mouse events through `/dev/uinput` as a virtual mouse.

This works on Wayland because it operates below the compositor at the evdev/uinput layer.

## Build

```bash
cargo build --release
```

The binary will be at:

```bash
target/release/mousedb
```

## Find your mouse event node

Prefer the stable by-id path:

```bash
ls -l /dev/input/by-id/*event-mouse
```

If unsure, install/run `evtest` and pick the device that prints events when you click your real mouse:

```bash
sudo evtest
```

Do **not** choose the virtual device named `debounced virtual mouse`.

## Run

```bash
sudo ./target/release/mousedb /dev/input/by-id/YOUR_MOUSE-event-mouse 80
```

`80` is the debounce threshold in milliseconds. Good values are usually `50` to `120`.

## Autostart with systemd

Copy the binary somewhere stable, for example:

```bash
sudo install -m755 target/release/mousedb /usr/local/bin/mousedb
```

Create `/etc/systemd/system/faulty-mouse-debounce.service`:

```ini
[Unit]
Description=Debounce faulty mouse left click
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/bin/mousedb /dev/input/by-id/YOUR_MOUSE-event-mouse 80
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Enable it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now faulty-mouse-debounce.service
```

## Stop / uninstall

```bash
sudo systemctl disable --now faulty-mouse-debounce.service
sudo rm /etc/systemd/system/faulty-mouse-debounce.service
sudo systemctl daemon-reload
```
