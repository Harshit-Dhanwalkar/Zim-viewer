# Zim-viewer

## Installation

To get started, clone the repository and its submodules, install the necessary dependencies, and then build and run the project.

```bash
git clone --recurse-submodules https://github.com/Harshit-Dhanwalkar/Zim-viewer.git

sudo apt install libzim-dev

cd Zim-viewer
cargo build
cargo run
```

### Alternative Installation

If you've already cloned the repository without the submodules, you can initialize and update them separately:

```bash
git clone https://github.com/Harshit-Dhanwalkar/Zim-viewer.git
cd Zim-viewer

sudo apt install libzim-dev

git submodule update --init

cargo build
cargo run
```
