Put local font files here before building.

Tracked in git:
- Geist-Regular.ttf
- GeistMono-Regular.ttf

Ignored / downloaded before build:
- SourceHanSans.ttc

SourceHanSans.ttc is ignored because the CJK collection is too large for normal GitHub Git.

Run this before local release builds:

    ./scripts/fetch-fonts.sh

Licenses are stored in:

    assets/licenses/LICENSE-Geist.txt
    assets/licenses/LICENSE-SourceHanSans.txt