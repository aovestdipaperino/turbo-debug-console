// Copyright (c) 2026 Enzo Lombardi
// SPDX-License-Identifier: MIT

//! Application command ids.
//!
//! Turbo Vision reserves the low range for built-ins (`CM_QUIT`, `CM_CLOSE`,
//! `CM_TILE`, ...). These start at 1000 to stay clear of them.

use turbo_vision::core::command::CommandId;

/// File > Open capture...
pub const CM_OPEN_CAPTURE: CommandId = 1000;
/// File > Save As...
pub const CM_SAVE_AS: CommandId = 1001;
/// Edit > Clear window
pub const CM_CLEAR_WINDOW: CommandId = 1004;
/// Edit > Copy (copies the current selection to the clipboard)
pub const CM_COPY: CommandId = 1002;
/// Edit > Select All (selects the whole scrollback)
pub const CM_SELECT_ALL: CommandId = 1003;
