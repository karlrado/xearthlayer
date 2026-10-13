# Package Management

XEarthLayer's package manager lets you install, update, and remove pre-built scenery packages. This is the easiest way to get high-quality satellite imagery for specific regions.

## Overview

Scenery packages are distributed through **package libraries** - index files that list available packages and their download locations. To use packages, you need:

1. A library URL from a scenery provider
2. XEarthLayer configured with that library URL

## Configuration

Set your library URL in `~/.config/xearthlayer/config.ini`:

```ini
[xplane]
scenery_dir = /path/to/X-Plane 12/Custom Scenery

[packages]
# library_url defaults to https://xearthlayer.app/packages/xearthlayer_package_library.txt
install_location = ~/.local/share/xearthlayer/packages
auto_install_overlays = true
```

The default `library_url` points to the official XEarthLayer package library, so most users don't need to configure this.

### Auto-Install Overlays

When `auto_install_overlays = true`, installing an ortho package will automatically install the matching overlay package for the same region (if available). This saves time when setting up new regions.

## Commands

### Selecting Packages by Region Code

Region codes are hierarchical, from continent to country to part of a
country, so `install`, `update`, `remove` and `info` all accept the start of a
code and act on every package it selects. Matching ignores case.

| You type | Selects |
|----------|---------|
| `NA` | every North America package |
| `NA-CANADA` | `NA-CANADA-WEST` and `NA-CANADA-EAST` |
| `AS-RUSSIA` | `AS-RUSSIA-WEST` and `AS-RUSSIA-EAST` |
| `AF-SOUTH` | exactly `AF-SOUTH` |

A code that selects one package behaves exactly as a full code always has.
When it selects several, the matches are listed first and you are asked to
confirm before anything is downloaded:

```
$ xearthlayer packages install NA
Installing NA (ortho)...

Fetching library index...
NA matches 4 packages (ortho):
  NA-CANADA-EAST v0.1.0
  NA-CANADA-WEST v0.1.0
  NA-GREENLAND v0.1.0
  NA-USA-MX-CENTRAL v0.1.1

Proceed with install? [y/N]:
```

If one package in the batch fails to install, the rest still install and the
command reports how many failed. A code that selects nothing lists the codes
you could have used: the library's for `install`, and your installed packages
for `update`, `remove` and `info`.

A full code is still the start of every longer code, so `EU` selects `EU`,
`EU-WEST` and `EU-EAST` together. There is no way to name only the shorter
one while the longer ones are installed.

### Check Available Packages

See what packages are available and their update status:

```bash
xearthlayer packages check
```

Output:
```
Checking for package updates...

  EU-PARIS (ortho) v1.0.0 - Not installed
  EU-ALPS (ortho) v2.0.0 - Installed (up to date)
  NA-SOCAL (ortho) v1.2.0 - Installed (update available: v1.3.0)

1 package(s) available for installation.
1 package(s) have updates available.
```

### List Installed Packages

View packages currently installed on your system:

```bash
xearthlayer packages list
```

Output:
```
Installed Packages (2)
======================

  EU-ALPS (ortho) v2.0.0 - 4.2 GB
  NA-SOCAL (ortho) v1.2.0 - 8.7 GB
```

For more details:

```bash
xearthlayer packages list --verbose
```

Output:
```
Installed Packages (2)
======================

EU-ALPS (ortho) v2.0.0
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-alps_ortho
  Size: 4.2 GB
  Mount: Not mounted

NA-SOCAL (ortho) v1.2.0
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_na-socal_ortho
  Size: 8.7 GB
  Mount: Not mounted
```

### Install a Package

Install a package by region code:

```bash
xearthlayer packages install eu-paris
```

For overlay packages (roads, buildings):

```bash
xearthlayer packages install eu-paris --type overlay
```

Output:
```
Installing eu-paris (ortho)...

Fetching library index...
Fetching package metadata...
Package: EU-PARIS v1.0.0
Parts: 3 (zzXEL_eu-paris_ortho-1.0.0.tar.gz)

Downloading part 1/3... [=========>          ] 45%
Downloading part 2/3... [====================] 100%
Downloading part 3/3... [====================] 100%

Verifying checksums...
Extracting...

Success: Installed EU-PARIS (ortho) v1.0.0 to /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-paris_ortho
Downloaded 1.2 GB, extracted 4521 files
```

### Package Info

Get detailed information about an installed package:

```bash
xearthlayer packages info eu-paris
```

Output:
```
EU-PARIS (ortho) v1.0.0
=======================

Package Details:
  Title: EU-PARIS
  Type: Orthophoto (satellite imagery)
  Version: 1.0.0
  Mountpoint: zzXEL_eu-paris_ortho

Installation:
  Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-paris_ortho
  Size: 1.2 GB

Mount Status:
  Not mounted

Archive Parts:
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.aa (500 MB)
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ab (500 MB)
  zzXEL_eu-paris_ortho-1.0.0.tar.gz.ac (200 MB)
```

### Update Packages

Update a specific package:

```bash
xearthlayer packages update eu-paris
```

Update all packages with available updates:

```bash
xearthlayer packages update --all
```

The update process:
1. Downloads the new version
2. Removes the old version
3. Extracts the new version

### Remove a Package

Remove an installed package:

```bash
xearthlayer packages remove eu-paris
```

You'll be prompted for confirmation:

```
Package: EU-PARIS (ortho) v1.0.0
Path: /home/user/X-Plane 12/Custom Scenery/zzXEL_eu-paris_ortho
Size: 1.2 GB

Remove this package? [y/N]: y

Removing package...
Success: Removed eu-paris (ortho)
```

When a code selects several packages, every one of them is listed, and the
removal is checked as a whole before anything is deleted: if any selected
package is mounted, nothing is removed. Then a single prompt covers the set:

```
Are you sure you want to remove these 2 packages? [y/N]:
```

To skip the confirmation prompt:

```bash
xearthlayer packages remove eu-paris --force
```

`--force` skips only the prompt. A mounted package still stops the removal.

## Package Types

XEarthLayer supports two package types:

| Type | Description | Default |
|------|-------------|---------|
| `ortho` | Satellite/aerial imagery (orthophotos) | Yes |
| `overlay` | Roads, buildings, landmarks | No |

Specify the type with `--type`:

```bash
xearthlayer packages install eu-paris --type overlay
```

## CLI Reference

All package commands support these common options:

`<REGION>` is a region code or the start of one; see
[Selecting Packages by Region Code](#selecting-packages-by-region-code).

| Option | Description |
|--------|-------------|
| `--library-url <URL>` | Override the library URL from config |
| `--install-dir <PATH>` | Override the installation directory |
| `--type <TYPE>` | Package type: `ortho` or `overlay` |

### install

```bash
xearthlayer packages install <REGION> [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--type <TYPE>` | Package type (default: ortho) |
| `--temp-dir <PATH>` | Temporary download directory |

### update

```bash
xearthlayer packages update [REGION] [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--all` | Update all packages |
| `--type <TYPE>` | Package type filter |

### remove

```bash
xearthlayer packages remove <REGION> [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `-f, --force` | Skip confirmation prompt |
| `--type <TYPE>` | Package type (default: ortho) |

## Disk Space

Large packages are split into multiple parts for easier downloading. While a package installs, its temp directory holds the reassembled archive and the extracted tree together, which can take up to four times the size of the download. Temporary files are removed as soon as each package finishes, whether it succeeds or fails.

When a command installs or updates several packages, each package installs while the next one downloads, so a batch takes about as long as its downloads rather than its downloads plus its installs. That overlap needs room for one package's install and the next package's download at the same time. XEarthLayer checks the whole batch once, before downloading anything:

- If the overlap fits, packages download and install side by side.
- If only one package at a time fits, the batch runs one package at a time instead, exactly as earlier versions did.
- If not even that fits, nothing is downloaded and the command reports how much space it needs.

No external tools are needed to unpack a package: archives are extracted inside XEarthLayer itself.

## Network Issues

If a download fails:

1. The partial download is preserved
2. Re-running the install command will resume where it left off
3. Checksums are verified to ensure integrity

## X-Plane Integration

Installed packages appear in your Custom Scenery folder with names like:

- `zzXEL_eu-paris_ortho` (orthophoto)
- `yzXEL_eu-paris_overlay` (overlay)

### Package Installation Methods

**Ortho packages** are installed to the package directory (e.g., `~/.local/share/xearthlayer/packages/`) and mounted via FUSE to Custom Scenery when you run `xearthlayer run`.

**Overlay packages** are installed to the package directory, then a symlink is automatically created in Custom Scenery pointing to the installed package. This allows X-Plane to access the overlay files directly without FUSE.

### Running XEarthLayer

After installing packages, start XEarthLayer to mount them:

```bash
xearthlayer run
```

This automatically discovers all installed ortho packages and mounts them in Custom Scenery. See [Running the Service](running-service.md) for details.

### Scenery Load Order

The `zz` and `yz` prefixes ensure correct scenery loading order in X-Plane:

- Overlays (`yz`) load before orthophotos (`zz`)
- Both load after most other custom scenery

You may need to adjust your `scenery_packs.ini` order if you have conflicts with other scenery.
