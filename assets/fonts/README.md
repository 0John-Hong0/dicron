Put local font files here before building.

Tracked in git:
- Geist-Regular.ttf
- GeistMono-Regular.ttf

Ignored / downloaded before build:
- SourceHanSans-Medium.otf

SourceHanSans-Medium.otf is ignored because it is 16.5 MB, which is larger than
normal GitHub Git usage warrants. It is Adobe's official standalone "Source Han
Sans Medium" face from the 2.005R tag, downloaded and checksum-verified by the
fetch script. Every font is compiled into the binary with include_bytes!, so the
file must be present before Cargo runs.

Run this before any local build:

    ./scripts/fetch-fonts.sh

The script needs only curl and sha256sum. It is idempotent, and it replaces the
file if its checksum does not match the pinned font.

Licenses are stored in:

    assets/licenses/LICENSE-Geist.txt
    assets/licenses/LICENSE-SourceHanSans.txt
