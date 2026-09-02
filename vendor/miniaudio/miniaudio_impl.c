// SPDX-License-Identifier: GPL-2.0-or-later OR MIT-0

// Keep miniaudio's implementation isolated from the Doom headers. On Windows,
// miniaudio includes windows.h while Doom defines its own boolean type and a
// function-like LONG macro; both names collide with Windows SDK declarations.
#define MINIAUDIO_IMPLEMENTATION
#include "miniaudio.h"
