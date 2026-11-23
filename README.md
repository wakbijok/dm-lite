# dm-lite (release mirror)

dm-lite has moved to <https://github.com/Ost-in-Edhil/dm-lite>.

This repository holds no source code. It exists only because older `dmem` builds look here for
their updates (`dmem upgrade`). Each release here carries the same signed archives as the matching
release in the main repository, byte for byte. `dmem upgrade` checks every archive against the
release key built into the binary before it installs anything, so a file that is not signed with
that key is refused.

Source, issues and current documentation: <https://github.com/Ost-in-Edhil/dm-lite>
