# Managed installs

A host installer, such as the daimon Expert installer, owns the version set it installs and
can roll it back. `dmem upgrade` knows nothing about that set: it takes the newest release
and replaces the running binary. On a managed install it therefore refuses, says so, and
names the installer's own command. This page is for the people who write such an installer
and for the operators who run one.

## What `dmem upgrade` does on a managed install

It stops before it contacts any server or writes any file. It prints

    dmem upgrade refused: this install is managed by the daimon Expert installer (...): upgrade
    it with `daimon-expert upgrade`, not with `dmem upgrade`

to standard error and ends with exit status 3. Exit status 3 means "refused"; any other
failure of `dmem upgrade` ends with exit status 1. `--yes` only skips the question that comes
after a verified download, so it does not get past the fence, and neither does `--pre`.

The fence exists only in a build that has `dmem upgrade`, which is a build with the
`self-update` feature (the `dist` build has it).

## What marks an install as managed

Either of these does:

1. **The marker file `.daimon-managed` beside the real binary.** The marker is a file of that
   name in the directory that holds the real `dmem` file, after every symlink on the way to
   it is resolved. It can be empty. A symlink at that name counts even when it points
   nowhere, and so does a directory. A marker beside a symlink to `dmem`, in some other
   directory, does not count.
2. **`DM_MANAGED` set to `1`, `true`, `yes` or `on`** in the environment of the command, in
   any case and with spaces around it ignored. Any other value is ignored.

**The marker is the fence that holds.** The installer writes it root-owned into a root-owned
directory, beside the binary it installed, so nobody who cannot write there can remove it.
The variable is a convenience: it reaches only the command it is set for. A service unit's
`Environment=` line does not put it into an operator's shell, so a person who types
`dmem upgrade` is stopped by the marker and not by the variable.

For the installer that places `dmem` in `/opt/daimon/<set version>/dmem/`: write the marker
to `/opt/daimon/<set version>/dmem/.daimon-managed`, in the same directory as `dmem` and
`libzvec_c_api.so`, owned by root and not writable by the service account. Do not put it
beside a symlink such as `/usr/local/bin/dmem`: the check follows the symlink and looks next
to the real file.

## When it cannot tell

The fence fails closed. `dmem upgrade` also refuses, with exit status 3, when

- the path of the running program cannot be found or resolved (for example, the file was
  deleted or replaced while it ran), or
- looking for the marker fails with anything other than "no such file", such as a directory
  the user may not search.

The message then says what failed instead of naming the marker. Only a lookup that finds
nothing, with `DM_MANAGED` unset or not one of the values above, lets the upgrade go on to
its usual checks.

## What the fence does not do

- It does not stop someone who can write the install directory from deleting the marker, or
  who can replace the binary by hand. A root-owned directory is the protection against that.
- It does not change the signature checks. `dmem upgrade` on an unmanaged install still
  installs a release only when its signature verifies against the key compiled in.
- It does not touch `dmem serve`, `dmem service` or any other command.
