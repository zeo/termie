# Third-party notices

termie is dual-licensed under MIT OR Apache-2.0 (see `LICENSE-MIT` and
`LICENSE-APACHE`). It bundles and depends on third-party work covered by their
own licenses, noted below.

## Bundled fonts (`assets/fonts/`)

### Maple Mono (Nerd Font variant)

Copyright (c) 2022 subframe7536.

Licensed under the SIL Open Font License, Version 1.1, reproduced in
`assets/fonts/OFL.txt`. Under the OFL the font may be bundled and redistributed
with software provided this notice and the license are included; it must not be
sold by itself.

The "NF" variant is patched with icon glyphs aggregated by the Nerd Fonts
project (https://github.com/ryanoasis/nerd-fonts). The constituent icon sets
carry their own permissive licenses (MIT, OFL, and others) as documented by
that project.

### Source Sans 3 (Regular, Medium)

Copyright 2010-2022 Adobe (http://www.adobe.com/), with Reserved Font Name
'Source'. Source is a trademark of Adobe in the United States and/or other
countries.

Licensed under the SIL Open Font License, Version 1.1, reproduced in
`assets/fonts/SourceSans3-LICENSE.txt`. termie uses it unmodified for interface
labels, menus, settings and dialogs.

## Bundled ConPTY host (`conpty.dll`, `OpenConsole.exe`)

Copyright (c) Microsoft Corporation.

Built from the Windows Terminal / console source at
https://github.com/microsoft/terminal and distributed by Microsoft as the
`Microsoft.Windows.Console.ConPTY` NuGet package, under the MIT License.
termie ships the pair beside its executable so terminal sessions run on a
current ConPTY host instead of the older one built into Windows.

## Rust dependencies

termie links a number of Rust crates (anyhow, bytemuck, cosmic-text,
ed25519-dalek, log, png, pollster, portable-pty, vte, wgpu, windows, winit, and
their transitive dependencies), each under permissive licenses (MIT,
Apache-2.0, BSD-3-Clause, and similar). ed25519-dalek, curve25519-dalek and
subtle are Copyright (c) 2016-2024 Isis Agora Lovecruft and Henry de Valence,
under BSD-3-Clause.
Their full license texts can be regenerated from the dependency tree with
`cargo about` or `cargo deny`.
