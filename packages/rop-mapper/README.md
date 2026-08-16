# RoP Mapper

`smudgy://official/rop-mapper` maps Rites of Passage from its authoritative
`Room.Info` exits and `Room.Map` neighborhoods.

New zones begin in durable local storage and survive restarts on this device.
Run `ropmap upgrade` to move every zone mapped by the package into cloud storage
and continue mapping with cloud autosave. To upgrade only one zone, append its
name, for example `ropmap upgrade Rites of Passage`.

An upgraded cloud map is resumed by zone name in later sessions. Rename a map if
you want the package to leave it alone.
