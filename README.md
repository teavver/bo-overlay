# bo

## Build

```bash
cargo build --release
```

## Install

```bash
sudo ln -s "$(pwd)/target/release/bo" /usr/bin/overlay
```
## Run

Default config: ~/.bo-config.toml

```
overlay FILE --config=./config.toml
```
