/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

/**
 * Provides cross-browser and cross-keyboard keyboard for a specific element.
 * Browser and keyboard layout variation is abstracted away, providing events
 * which represent keys as their corresponding X11 keysym.
 *
 * @constructor
 * @param {Element} element The Element to use to provide keyboard events.
 */
Keyboard = function(element) {

    /**
     * Reference to this Keyboard.
     * @private
     */
    var guac_keyboard = this;

    /**
     * Fired whenever the user presses a key with the element associated
     * with this Keyboard in focus.
     *
     * @event
     * @param {Number} keysym The keysym of the key being pressed.
     * @param {String} modifier_state The string representation of
     *                                all active modifiers.
     * @return {Boolean} true if the key event should be allowed through to the
     *                   browser, false otherwise.
     */
    this.onkeydown = null;

    /**
     * Fired whenever the user releases a key with the element associated
     * with this Keyboard in focus.
     *
     * @event
     * @param {Number} keysym The keysym of the key being released.
     * @param {String} modifier_state The string representation of
     *                                all active modifiers.
     */
    this.onkeyup = null;

    /**
     * A key event having a corresponding timestamp. This event is non-specific.
     * Its subclasses should be used instead when recording specific key
     * events.
     *
     * @private
     * @constructor
     */
    var KeyEvent = function() {

        /**
         * Reference to this key event.
         */
        var key_event = this;

        /**
         * An arbitrary timestamp in milliseconds, indicating this event's
         * position in time relative to other events.
         *
         * @type {Number}
         */
        this.timestamp = new Date().getTime();

        /**
         * Whether the default action of this key event should be prevented.
         *
         * @type {Boolean}
         */
        this.defaultPrevented = false;

        /**
         * The keysym of the key associated with this key event, as determined
         * by a best-effort guess using available event properties and keyboard
         * state.
         *
         * @type {Number}
         */
        this.keysym = null;

        /**
         * Whether the keysym value of this key event is known to be reliable.
         * If false, the keysym may still be valid, but it's only a best guess,
         * and future key events may be a better source of information.
         *
         * @type {Boolean}
         */
        this.reliable = false;

        /**
         * Returns the number of milliseconds elapsed since this event was
         * received.
         *
         * @return {Number} The number of milliseconds elapsed since this
         *                  event was received.
         */
        this.getAge = function() {
            return new Date().getTime() - key_event.timestamp;
        };

    };

    /**
     * Information related to the pressing of a key, which need not be a key
     * associated with a printable character. The presence or absence of any
     * information within this object is browser-dependent.
     *
     * @private
     * @constructor
     * @augments Keyboard.KeyEvent
     * @param {Number} keyCode The JavaScript key code of the key pressed.
     * @param {String} keyIdentifier The legacy DOM3 "keyIdentifier" of the key
     *                               pressed, as defined at:
     *                               http://www.w3.org/TR/2009/WD-DOM-Level-3-Events-20090908/#events-Events-KeyboardEvent
     * @param {String} key The standard name of the key pressed, as defined at:
     *                     http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
     * @param {Number} location The location on the keyboard corresponding to
     *                          the key pressed, as defined at:
     *                          http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
     */
    var KeydownEvent = function(keyCode, keyIdentifier, key, location) {

        // We extend KeyEvent
        KeyEvent.apply(this);

        /**
         * The JavaScript key code of the key pressed.
         *
         * @type {Number}
         */
        this.keyCode = keyCode;

        /**
         * The legacy DOM3 "keyIdentifier" of the key pressed, as defined at:
         * http://www.w3.org/TR/2009/WD-DOM-Level-3-Events-20090908/#events-Events-KeyboardEvent
         *
         * @type {String}
         */
        this.keyIdentifier = keyIdentifier;

        /**
         * The standard name of the key pressed, as defined at:
         * http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
         *
         * @type {String}
         */
        this.key = key;

        /**
         * The location on the keyboard corresponding to the key pressed, as
         * defined at:
         * http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
         *
         * @type {Number}
         */
        this.location = location;

        // If key is known from keyCode or DOM3 alone, use that
        this.keysym =  keysym_from_key_identifier(key, location)
                    || keysym_from_keycode(keyCode, location);

        // DOM3 and keyCode are reliable sources if the corresponding key is
        // not a printable key
        if (this.keysym && !isPrintable(this.keysym))
            this.reliable = true;

        // Use legacy keyIdentifier as a last resort, if it looks sane
        if (!this.keysym && key_identifier_sane(keyCode, keyIdentifier))
            this.keysym = keysym_from_key_identifier(keyIdentifier, location, guac_keyboard.modifiers.shift);

        // Determine whether default action for Alt+combinations must be prevented
        var prevent_alt =  !guac_keyboard.modifiers.ctrl
                        && !(navigator && navigator.platform && navigator.platform.match(/^mac/i));

        // Determine whether default action for Ctrl+combinations must be prevented
        var prevent_ctrl = !guac_keyboard.modifiers.alt;

        // We must rely on the (potentially buggy) keyIdentifier if preventing
        // the default action is important
        if ((prevent_ctrl && guac_keyboard.modifiers.ctrl)
         || (prevent_alt  && guac_keyboard.modifiers.alt)
         || guac_keyboard.modifiers.meta
         || guac_keyboard.modifiers.hyper)
            this.reliable = true;

        // Record most recently known keysym by associated key code
        recentKeysym[keyCode] = this.keysym;

    };

    KeydownEvent.prototype = new KeyEvent();

    /**
     * Information related to the pressing of a key, which MUST be
     * associated with a printable character. The presence or absence of any
     * information within this object is browser-dependent.
     *
     * @private
     * @constructor
     * @augments Keyboard.KeyEvent
     * @param {Number} charCode The Unicode codepoint of the character that
     *                          would be typed by the key pressed.
     */
    var KeypressEvent = function(charCode) {

        // We extend KeyEvent
        KeyEvent.apply(this);

        /**
         * The Unicode codepoint of the character that would be typed by the
         * key pressed.
         *
         * @type {Number}
         */
        this.charCode = charCode;

        // Pull keysym from char code
        this.keysym = keysym_from_charcode(charCode);

        // Keypress is always reliable
        this.reliable = true;

    };

    KeypressEvent.prototype = new KeyEvent();

    /**
     * Information related to the pressing of a key, which need not be a key
     * associated with a printable character. The presence or absence of any
     * information within this object is browser-dependent.
     *
     * @private
     * @constructor
     * @augments Keyboard.KeyEvent
     * @param {Number} keyCode The JavaScript key code of the key released.
     * @param {String} keyIdentifier The legacy DOM3 "keyIdentifier" of the key
     *                               released, as defined at:
     *                               http://www.w3.org/TR/2009/WD-DOM-Level-3-Events-20090908/#events-Events-KeyboardEvent
     * @param {String} key The standard name of the key released, as defined at:
     *                     http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
     * @param {Number} location The location on the keyboard corresponding to
     *                          the key released, as defined at:
     *                          http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
     */
    var KeyupEvent = function(keyCode, keyIdentifier, key, location) {

        // We extend KeyEvent
        KeyEvent.apply(this);

        /**
         * The JavaScript key code of the key released.
         *
         * @type {Number}
         */
        this.keyCode = keyCode;

        /**
         * The legacy DOM3 "keyIdentifier" of the key released, as defined at:
         * http://www.w3.org/TR/2009/WD-DOM-Level-3-Events-20090908/#events-Events-KeyboardEvent
         *
         * @type {String}
         */
        this.keyIdentifier = keyIdentifier;

        /**
         * The standard name of the key released, as defined at:
         * http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
         *
         * @type {String}
         */
        this.key = key;

        /**
         * The location on the keyboard corresponding to the key released, as
         * defined at:
         * http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
         *
         * @type {Number}
         */
        this.location = location;

        // If key is known from keyCode or DOM3 alone, use that
        this.keysym =  recentKeysym[keyCode]
                    || keysym_from_keycode(keyCode, location)
                    || keysym_from_key_identifier(key, location); // keyCode is still more reliable for keyup when dead keys are in use

        // Keyup is as reliable as it will ever be
        this.reliable = true;

    };

    KeyupEvent.prototype = new KeyEvent();

    /**
     * An array of recorded events, which can be instances of the private
     * KeydownEvent, KeypressEvent, and KeyupEvent classes.
     *
     * @private
     * @type {KeyEvent[]}
     */
    var eventLog = [];

    /**
     * Map of known JavaScript keycodes which do not map to typable characters
     * to their X11 keysym equivalents.
     * @private
     */
    var keycodeKeysyms = {
        8:   [0xFF08], // backspace
        9:   [0xFF09], // tab
        12:  [0xFF0B, 0xFF0B, 0xFF0B, 0xFFB5], // clear       / KP 5
        13:  [0xFF0D], // enter
        16:  [0xFFE1, 0xFFE1, 0xFFE2], // shift
        17:  [0xFFE3, 0xFFE3, 0xFFE4], // ctrl
        18:  [0xFFE9, 0xFFE9, 0xFE03], // alt
        19:  [0xFF13], // pause/break
        20:  [0xFFE5], // caps lock
        27:  [0xFF1B], // escape
        32:  [0x0020], // space
        33:  [0xFF55, 0xFF55, 0xFF55, 0xFFB9], // page up     / KP 9
        34:  [0xFF56, 0xFF56, 0xFF56, 0xFFB3], // page down   / KP 3
        35:  [0xFF57, 0xFF57, 0xFF57, 0xFFB1], // end         / KP 1
        36:  [0xFF50, 0xFF50, 0xFF50, 0xFFB7], // home        / KP 7
        37:  [0xFF51, 0xFF51, 0xFF51, 0xFFB4], // left arrow  / KP 4
        38:  [0xFF52, 0xFF52, 0xFF52, 0xFFB8], // up arrow    / KP 8
        39:  [0xFF53, 0xFF53, 0xFF53, 0xFFB6], // right arrow / KP 6
        40:  [0xFF54, 0xFF54, 0xFF54, 0xFFB2], // down arrow  / KP 2
        45:  [0xFF63, 0xFF63, 0xFF63, 0xFFB0], // insert      / KP 0
        46:  [0xFFFF, 0xFFFF, 0xFFFF, 0xFFAE], // delete      / KP decimal
        91:  [0xFFEB], // left window key (hyper_l)
        92:  [0xFF67], // right window key (menu key?)
        93:  null,     // select key
        96:  [0xFFB0], // KP 0
        97:  [0xFFB1], // KP 1
        98:  [0xFFB2], // KP 2
        99:  [0xFFB3], // KP 3
        100: [0xFFB4], // KP 4
        101: [0xFFB5], // KP 5
        102: [0xFFB6], // KP 6
        103: [0xFFB7], // KP 7
        104: [0xFFB8], // KP 8
        105: [0xFFB9], // KP 9
        106: [0xFFAA], // KP multiply
        107: [0xFFAB], // KP add
        109: [0xFFAD], // KP subtract
        110: [0xFFAE], // KP decimal
        111: [0xFFAF], // KP divide
        112: [0xFFBE], // f1
        113: [0xFFBF], // f2
        114: [0xFFC0], // f3
        115: [0xFFC1], // f4
        116: [0xFFC2], // f5
        117: [0xFFC3], // f6
        118: [0xFFC4], // f7
        119: [0xFFC5], // f8
        120: [0xFFC6], // f9
        121: [0xFFC7], // f10
        122: [0xFFC8], // f11
        123: [0xFFC9], // f12
        144: [0xFF7F], // num lock
        145: [0xFF14], // scroll lock
        225: [0xFE03]  // altgraph (iso_level3_shift)
    };

    /**
     * Map of known JavaScript keyidentifiers which do not map to typable
     * characters to their unshifted X11 keysym equivalents.
     * @private
     */
    var keyidentifier_keysym = {
        "Again": [0xFF66],
        "AllCandidates": [0xFF3D],
        "Alphanumeric": [0xFF30],
        "Alt": [0xFFE9, 0xFFE9, 0xFE03],
        "Attn": [0xFD0E],
        "AltGraph": [0xFE03],
        "ArrowDown": [0xFF54],
        "ArrowLeft": [0xFF51],
        "ArrowRight": [0xFF53],
        "ArrowUp": [0xFF52],
        "Backspace": [0xFF08],
        "CapsLock": [0xFFE5],
        "Cancel": [0xFF69],
        "Clear": [0xFF0B],
        "Convert": [0xFF21],
        "Copy": [0xFD15],
        "Crsel": [0xFD1C],
        "CrSel": [0xFD1C],
        "CodeInput": [0xFF37],
        "Compose": [0xFF20],
        "Control": [0xFFE3, 0xFFE3, 0xFFE4],
        "ContextMenu": [0xFF67],
        "DeadGrave": [0xFE50],
        "DeadAcute": [0xFE51],
        "DeadCircumflex": [0xFE52],
        "DeadTilde": [0xFE53],
        "DeadMacron": [0xFE54],
        "DeadBreve": [0xFE55],
        "DeadAboveDot": [0xFE56],
        "DeadUmlaut": [0xFE57],
        "DeadAboveRing": [0xFE58],
        "DeadDoubleacute": [0xFE59],
        "DeadCaron": [0xFE5A],
        "DeadCedilla": [0xFE5B],
        "DeadOgonek": [0xFE5C],
        "DeadIota": [0xFE5D],
        "DeadVoicedSound": [0xFE5E],
        "DeadSemivoicedSound": [0xFE5F],
        "Delete": [0xFFFF],
        "Down": [0xFF54],
        "End": [0xFF57],
        "Enter": [0xFF0D],
        "EraseEof": [0xFD06],
        "Escape": [0xFF1B],
        "Execute": [0xFF62],
        "Exsel": [0xFD1D],
        "ExSel": [0xFD1D],
        "F1": [0xFFBE],
        "F2": [0xFFBF],
        "F3": [0xFFC0],
        "F4": [0xFFC1],
        "F5": [0xFFC2],
        "F6": [0xFFC3],
        "F7": [0xFFC4],
        "F8": [0xFFC5],
        "F9": [0xFFC6],
        "F10": [0xFFC7],
        "F11": [0xFFC8],
        "F12": [0xFFC9],
        "F13": [0xFFCA],
        "F14": [0xFFCB],
        "F15": [0xFFCC],
        "F16": [0xFFCD],
        "F17": [0xFFCE],
        "F18": [0xFFCF],
        "F19": [0xFFD0],
        "F20": [0xFFD1],
        "F21": [0xFFD2],
        "F22": [0xFFD3],
        "F23": [0xFFD4],
        "F24": [0xFFD5],
        "Find": [0xFF68],
        "GroupFirst": [0xFE0C],
        "GroupLast": [0xFE0E],
        "GroupNext": [0xFE08],
        "GroupPrevious": [0xFE0A],
        "FullWidth": null,
        "HalfWidth": null,
        "HangulMode": [0xFF31],
        "Hankaku": [0xFF29],
        "HanjaMode": [0xFF34],
        "Help": [0xFF6A],
        "Hiragana": [0xFF25],
        "HiraganaKatakana": [0xFF27],
        "Home": [0xFF50],
        "Hyper": [0xFFED, 0xFFED, 0xFFEE],
        "Insert": [0xFF63],
        "JapaneseHiragana": [0xFF25],
        "JapaneseKatakana": [0xFF26],
        "JapaneseRomaji": [0xFF24],
        "JunjaMode": [0xFF38],
        "KanaMode": [0xFF2D],
        "KanjiMode": [0xFF21],
        "Katakana": [0xFF26],
        "Left": [0xFF51],
        "Meta": [0xFFE7, 0xFFE7, 0xFFE8],
        "ModeChange": [0xFF7E],
        "NumLock": [0xFF7F],
        "PageDown": [0xFF56],
        "PageUp": [0xFF55],
        "Pause": [0xFF13],
        "Play": [0xFD16],
        "PreviousCandidate": [0xFF3E],
        "PrintScreen": [0xFD1D],
        "Redo": [0xFF66],
        "Right": [0xFF53],
        "RomanCharacters": null,
        "Scroll": [0xFF14],
        "Select": [0xFF60],
        "Separator": [0xFFAC],
        "Shift": [0xFFE1, 0xFFE1, 0xFFE2],
        "SingleCandidate": [0xFF3C],
        "Super": [0xFFEB, 0xFFEB, 0xFFEC],
        "Tab": [0xFF09],
        "Up": [0xFF52],
        "Undo": [0xFF65],
        "Win": [0xFFEB],
        "Zenkaku": [0xFF28],
        "ZenkakuHankaku": [0xFF2A]
    };

    const keysym_to_string = {
        0xFFFFFF: "VoidSymbol",
        0xFF08: "BackSpace",
        0xFF09: "Tab",
        0xFF0A: "Linefeed",
        0xFF0B: "Clear",
        0xFF0D: "Return",
        0xFF13: "Pause",
        0xFF14: "Scroll_Lock",
        0xFF15: "Sys_Req",
        0xFF1B: "Escape",
        0xFFFF: "Delete",
        0xFF20: "Multi_key",
        0xFF37: "Codeinput",
        0xFF3C: "SingleCandidate",
        0xFF3D: "MultipleCandidate",
        0xFF3E: "PreviousCandidate",
        0xFF21: "Kanji",
        0xFF22: "Muhenkan",
        0xFF23: "Henkan_Mode",
        0xFF23: "Henkan",
        0xFF24: "Romaji",
        0xFF25: "Hiragana",
        0xFF26: "Katakana",
        0xFF27: "Hiragana_Katakana",
        0xFF28: "Zenkaku",
        0xFF29: "Hankaku",
        0xFF2A: "Zenkaku_Hankaku",
        0xFF2B: "Touroku",
        0xFF2C: "Massyo",
        0xFF2D: "Kana_Lock",
        0xFF2E: "Kana_Shift",
        0xFF2F: "Eisu_Shift",
        0xFF30: "Eisu_toggle",
        0xFF37: "Kanji_Bangou",
        0xFF3D: "Zen_Koho",
        0xFF3E: "Mae_Koho",
        0xFF50: "Home",
        0xFF51: "Left",
        0xFF52: "Up",
        0xFF53: "Right",
        0xFF54: "Down",
        0xFF55: "Prior",
        0xFF55: "Page_Up",
        0xFF56: "Next",
        0xFF56: "Page_Down",
        0xFF57: "End",
        0xFF58: "Begin",
        0xFF60: "Select",
        0xFF61: "Print",
        0xFF62: "Execute",
        0xFF63: "Insert",
        0xFF65: "Undo",
        0xFF66: "Redo",
        0xFF67: "Menu",
        0xFF68: "Find",
        0xFF69: "Cancel",
        0xFF6A: "Help",
        0xFF6B: "Break",
        0xFF7E: "Mode_switch",
        0xFF7E: "script_switch",
        0xFF7F: "Num_Lock",
        0xFF80: "KP_Space",
        0xFF89: "KP_Tab",
        0xFF8D: "KP_Enter",
        0xFF91: "KP_F1",
        0xFF92: "KP_F2",
        0xFF93: "KP_F3",
        0xFF94: "KP_F4",
        0xFF95: "KP_Home",
        0xFF96: "KP_Left",
        0xFF97: "KP_Up",
        0xFF98: "KP_Right",
        0xFF99: "KP_Down",
        0xFF9A: "KP_Prior",
        0xFF9A: "KP_Page_Up",
        0xFF9B: "KP_Next",
        0xFF9B: "KP_Page_Down",
        0xFF9C: "KP_End",
        0xFF9D: "KP_Begin",
        0xFF9E: "KP_Insert",
        0xFF9F: "KP_Delete",
        0xFFBD: "KP_Equal",
        0xFFAA: "KP_Multiply",
        0xFFAB: "KP_Add",
        0xFFAC: "KP_Separator",
        0xFFAD: "KP_Subtract",
        0xFFAE: "KP_Decimal",
        0xFFAF: "KP_Divide",
        0xFFB0: "KP_0",
        0xFFB1: "KP_1",
        0xFFB2: "KP_2",
        0xFFB3: "KP_3",
        0xFFB4: "KP_4",
        0xFFB5: "KP_5",
        0xFFB6: "KP_6",
        0xFFB7: "KP_7",
        0xFFB8: "KP_8",
        0xFFB9: "KP_9",
        0xFFBE: "F1",
        0xFFBF: "F2",
        0xFFC0: "F3",
        0xFFC1: "F4",
        0xFFC2: "F5",
        0xFFC3: "F6",
        0xFFC4: "F7",
        0xFFC5: "F8",
        0xFFC6: "F9",
        0xFFC7: "F10",
        0xFFC8: "F11",
        0xFFC8: "L1",
        0xFFC9: "F12",
        0xFFC9: "L2",
        0xFFCA: "F13",
        0xFFCA: "L3",
        0xFFCB: "F14",
        0xFFCB: "L4",
        0xFFCC: "F15",
        0xFFCC: "L5",
        0xFFCD: "F16",
        0xFFCD: "L6",
        0xFFCE: "F17",
        0xFFCE: "L7",
        0xFFCF: "F18",
        0xFFCF: "L8",
        0xFFD0: "F19",
        0xFFD0: "L9",
        0xFFD1: "F20",
        0xFFD1: "L10",
        0xFFD2: "F21",
        0xFFD2: "R1",
        0xFFD3: "F22",
        0xFFD3: "R2",
        0xFFD4: "F23",
        0xFFD4: "R3",
        0xFFD5: "F24",
        0xFFD5: "R4",
        0xFFD6: "F25",
        0xFFD6: "R5",
        0xFFD7: "F26",
        0xFFD7: "R6",
        0xFFD8: "F27",
        0xFFD8: "R7",
        0xFFD9: "F28",
        0xFFD9: "R8",
        0xFFDA: "F29",
        0xFFDA: "R9",
        0xFFDB: "F30",
        0xFFDB: "R10",
        0xFFDC: "F31",
        0xFFDC: "R11",
        0xFFDD: "F32",
        0xFFDD: "R12",
        0xFFDE: "F33",
        0xFFDE: "R13",
        0xFFDF: "F34",
        0xFFDF: "R14",
        0xFFE0: "F35",
        0xFFE0: "R15",
        0xFFE1: "Shift_L",
        0xFFE2: "Shift_R",
        0xFFE3: "Control_L",
        0xFFE4: "Control_R",
        0xFFE5: "Caps_Lock",
        0xFFE6: "Shift_Lock",
        0xFFE7: "Meta_L",
        0xFFE8: "Meta_R",
        0xFFE9: "Alt_L",
        0xFFEA: "Alt_R",
        0xFFEB: "Super_L",
        0xFFEC: "Super_R",
        0xFFED: "Hyper_L",
        0xFFEE: "Hyper_R",
        0xFE01: "ISO_Lock",
        0xFE02: "ISO_Level2_Latch",
        0xFE03: "ISO_Level3_Shift",
        0xFE04: "ISO_Level3_Latch",
        0xFE05: "ISO_Level3_Lock",
        0xFE11: "ISO_Level5_Shift",
        0xFE12: "ISO_Level5_Latch",
        0xFE13: "ISO_Level5_Lock",
        0xFF7E: "ISO_Group_Shift",
        0xFE06: "ISO_Group_Latch",
        0xFE07: "ISO_Group_Lock",
        0xFE08: "ISO_Next_Group",
        0xFE09: "ISO_Next_Group_Lock",
        0xFE0A: "ISO_Prev_Group",
        0xFE0B: "ISO_Prev_Group_Lock",
        0xFE0C: "ISO_First_Group",
        0xFE0D: "ISO_First_Group_Lock",
        0xFE0E: "ISO_Last_Group",
        0xFE0F: "ISO_Last_Group_Lock",
        0xFE20: "ISO_Left_Tab",
        0xFE21: "ISO_Move_Line_Up",
        0xFE22: "ISO_Move_Line_Down",
        0xFE23: "ISO_Partial_Line_Up",
        0xFE24: "ISO_Partial_Line_Down",
        0xFE25: "ISO_Partial_Space_Left",
        0xFE26: "ISO_Partial_Space_Right",
        0xFE27: "ISO_Set_Margin_Left",
        0xFE28: "ISO_Set_Margin_Right",
        0xFE29: "ISO_Release_Margin_Left",
        0xFE2A: "ISO_Release_Margin_Right",
        0xFE2B: "ISO_Release_Both_Margins",
        0xFE2C: "ISO_Fast_Cursor_Left",
        0xFE2D: "ISO_Fast_Cursor_Right",
        0xFE2E: "ISO_Fast_Cursor_Up",
        0xFE2F: "ISO_Fast_Cursor_Down",
        0xFE30: "ISO_Continuous_Underline",
        0xFE31: "ISO_Discontinuous_Underline",
        0xFE32: "ISO_Emphasize",
        0xFE33: "ISO_Center_Object",
        0xFE34: "ISO_Enter",
        0xFE50: "dead_grave",
        0xFE51: "dead_acute",
        0xFE52: "dead_circumflex",
        0xFE53: "dead_tilde",
        0xFE53: "dead_perispomeni",
        0xFE54: "dead_macron",
        0xFE55: "dead_breve",
        0xFE56: "dead_abovedot",
        0xFE57: "dead_diaeresis",
        0xFE58: "dead_abovering",
        0xFE59: "dead_doubleacute",
        0xFE5A: "dead_caron",
        0xFE5B: "dead_cedilla",
        0xFE5C: "dead_ogonek",
        0xFE5D: "dead_iota",
        0xFE5E: "dead_voiced_sound",
        0xFE5F: "dead_semivoiced_sound",
        0xFE60: "dead_belowdot",
        0xFE61: "dead_hook",
        0xFE62: "dead_horn",
        0xFE63: "dead_stroke",
        0xFE64: "dead_abovecomma",
        0xFE64: "dead_psili",
        0xFE65: "dead_abovereversedcomma",
        0xFE65: "dead_dasia",
        0xFE66: "dead_doublegrave",
        0xFE67: "dead_belowring",
        0xFE68: "dead_belowmacron",
        0xFE69: "dead_belowcircumflex",
        0xFE6A: "dead_belowtilde",
        0xFE6B: "dead_belowbreve",
        0xFE6C: "dead_belowdiaeresis",
        0xFE6D: "dead_invertedbreve",
        0xFE6E: "dead_belowcomma",
        0xFE6F: "dead_currency",
        0xFE90: "dead_lowline",
        0xFE91: "dead_aboveverticalline",
        0xFE92: "dead_belowverticalline",
        0xFE93: "dead_longsolidusoverlay",
        0xFE80: "dead_a",
        0xFE81: "dead_A",
        0xFE82: "dead_e",
        0xFE83: "dead_E",
        0xFE84: "dead_i",
        0xFE85: "dead_I",
        0xFE86: "dead_o",
        0xFE87: "dead_O",
        0xFE88: "dead_u",
        0xFE89: "dead_U",
        0xFE8A: "dead_small_schwa",
        0xFE8B: "dead_capital_schwa",
        0xFE8C: "dead_greek",
        0xFED0: "First_Virtual_Screen",
        0xFED1: "Prev_Virtual_Screen",
        0xFED2: "Next_Virtual_Screen",
        0xFED4: "Last_Virtual_Screen",
        0xFED5: "Terminate_Server",
        0xFE70: "AccessX_Enable",
        0xFE71: "AccessX_Feedback_Enable",
        0xFE72: "RepeatKeys_Enable",
        0xFE73: "SlowKeys_Enable",
        0xFE74: "BounceKeys_Enable",
        0xFE75: "StickyKeys_Enable",
        0xFE76: "MouseKeys_Enable",
        0xFE77: "MouseKeys_Accel_Enable",
        0xFE78: "Overlay1_Enable",
        0xFE79: "Overlay2_Enable",
        0xFE7A: "AudibleBell_Enable",
        0xFEE0: "Pointer_Left",
        0xFEE1: "Pointer_Right",
        0xFEE2: "Pointer_Up",
        0xFEE3: "Pointer_Down",
        0xFEE4: "Pointer_UpLeft",
        0xFEE5: "Pointer_UpRight",
        0xFEE6: "Pointer_DownLeft",
        0xFEE7: "Pointer_DownRight",
        0xFEE8: "Pointer_Button_Dflt",
        0xFEE9: "Pointer_Button1",
        0xFEEA: "Pointer_Button2",
        0xFEEB: "Pointer_Button3",
        0xFEEC: "Pointer_Button4",
        0xFEED: "Pointer_Button5",
        0xFEEE: "Pointer_DblClick_Dflt",
        0xFEEF: "Pointer_DblClick1",
        0xFEF0: "Pointer_DblClick2",
        0xFEF1: "Pointer_DblClick3",
        0xFEF2: "Pointer_DblClick4",
        0xFEF3: "Pointer_DblClick5",
        0xFEF4: "Pointer_Drag_Dflt",
        0xFEF5: "Pointer_Drag1",
        0xFEF6: "Pointer_Drag2",
        0xFEF7: "Pointer_Drag3",
        0xFEF8: "Pointer_Drag4",
        0xFEFD: "Pointer_Drag5",
        0xFEF9: "Pointer_EnableKeys",
        0xFEFA: "Pointer_Accelerate",
        0xFEFB: "Pointer_DfltBtnNext",
        0xFEFC: "Pointer_DfltBtnPrev",
        0xFEA0: "ch",
        0xFEA1: "Ch",
        0xFEA2: "CH",
        0xFEA3: "c_h",
        0xFEA4: "C_h",
        0xFEA5: "C_H",
        0xFD01: "3270_Duplicate",
        0xFD02: "3270_FieldMark",
        0xFD03: "3270_Right2",
        0xFD04: "3270_Left2",
        0xFD05: "3270_BackTab",
        0xFD06: "3270_EraseEOF",
        0xFD07: "3270_EraseInput",
        0xFD08: "3270_Reset",
        0xFD09: "3270_Quit",
        0xFD0A: "3270_PA1",
        0xFD0B: "3270_PA2",
        0xFD0C: "3270_PA3",
        0xFD0D: "3270_Test",
        0xFD0E: "3270_Attn",
        0xFD0F: "3270_CursorBlink",
        0xFD10: "3270_AltCursor",
        0xFD11: "3270_KeyClick",
        0xFD12: "3270_Jump",
        0xFD13: "3270_Ident",
        0xFD14: "3270_Rule",
        0xFD15: "3270_Copy",
        0xFD16: "3270_Play",
        0xFD17: "3270_Setup",
        0xFD18: "3270_Record",
        0xFD19: "3270_ChangeScreen",
        0xFD1A: "3270_DeleteWord",
        0xFD1B: "3270_ExSelect",
        0xFD1C: "3270_CursorSelect",
        0xFD1D: "3270_PrintScreen",
        0xFD1E: "3270_Enter",
        0x0020: "space",
        0x0021: "exclam",
        0x0022: "quotedbl",
        0x0023: "numbersign",
        0x0024: "dollar",
        0x0025: "percent",
        0x0026: "ampersand",
        0x0027: "apostrophe",
        0x0027: "quoteright",
        0x0028: "parenleft",
        0x0029: "parenright",
        0x002A: "asterisk",
        0x002B: "plus",
        0x002C: "comma",
        0x002D: "minus",
        0x002E: "period",
        0x002F: "slash",
        0x0030: "0",
        0x0031: "1",
        0x0032: "2",
        0x0033: "3",
        0x0034: "4",
        0x0035: "5",
        0x0036: "6",
        0x0037: "7",
        0x0038: "8",
        0x0039: "9",
        0x003A: "colon",
        0x003B: "semicolon",
        0x003C: "less",
        0x003D: "equal",
        0x003E: "greater",
        0x003F: "question",
        0x0040: "at",
        0x0041: "A",
        0x0042: "B",
        0x0043: "C",
        0x0044: "D",
        0x0045: "E",
        0x0046: "F",
        0x0047: "G",
        0x0048: "H",
        0x0049: "I",
        0x004A: "J",
        0x004B: "K",
        0x004C: "L",
        0x004D: "M",
        0x004E: "N",
        0x004F: "O",
        0x0050: "P",
        0x0051: "Q",
        0x0052: "R",
        0x0053: "S",
        0x0054: "T",
        0x0055: "U",
        0x0056: "V",
        0x0057: "W",
        0x0058: "X",
        0x0059: "Y",
        0x005A: "Z",
        0x005B: "bracketleft",
        0x005C: "backslash",
        0x005D: "bracketright",
        0x005E: "asciicircum",
        0x005F: "underscore",
        0x0060: "grave",
        0x0060: "quoteleft",
        0x0061: "a",
        0x0062: "b",
        0x0063: "c",
        0x0064: "d",
        0x0065: "e",
        0x0066: "f",
        0x0067: "g",
        0x0068: "h",
        0x0069: "i",
        0x006A: "j",
        0x006B: "k",
        0x006C: "l",
        0x006D: "m",
        0x006E: "n",
        0x006F: "o",
        0x0070: "p",
        0x0071: "q",
        0x0072: "r",
        0x0073: "s",
        0x0074: "t",
        0x0075: "u",
        0x0076: "v",
        0x0077: "w",
        0x0078: "x",
        0x0079: "y",
        0x007A: "z",
        0x007B: "braceleft",
        0x007C: "bar",
        0x007D: "braceright",
        0x007E: "asciitilde",
        0x00A0: "nobreakspace",
        0x00A1: "exclamdown",
        0x00A2: "cent",
        0x00A3: "sterling",
        0x00A4: "currency",
        0x00A5: "yen",
        0x00A6: "brokenbar",
        0x00A7: "section",
        0x00A8: "diaeresis",
        0x00A9: "copyright",
        0x00AA: "ordfeminine",
        0x00AB: "guillemotleft",
        0x00AC: "notsign",
        0x00AD: "hyphen",
        0x00AE: "registered",
        0x00AF: "macron",
        0x00B0: "degree",
        0x00B1: "plusminus",
        0x00B2: "twosuperior",
        0x00B3: "threesuperior",
        0x00B4: "acute",
        0x00B5: "mu",
        0x00B6: "paragraph",
        0x00B7: "periodcentered",
        0x00B8: "cedilla",
        0x00B9: "onesuperior",
        0x00BA: "masculine",
        0x00BB: "guillemotright",
        0x00BC: "onequarter",
        0x00BD: "onehalf",
        0x00BE: "threequarters",
        0x00BF: "questiondown",
        0x00C0: "Agrave",
        0x00C1: "Aacute",
        0x00C2: "Acircumflex",
        0x00C3: "Atilde",
        0x00C4: "Adiaeresis",
        0x00C5: "Aring",
        0x00C6: "AE",
        0x00C7: "Ccedilla",
        0x00C8: "Egrave",
        0x00C9: "Eacute",
        0x00CA: "Ecircumflex",
        0x00CB: "Ediaeresis",
        0x00CC: "Igrave",
        0x00CD: "Iacute",
        0x00CE: "Icircumflex",
        0x00CF: "Idiaeresis",
        0x00D0: "ETH",
        0x00D0: "Eth",
        0x00D1: "Ntilde",
        0x00D2: "Ograve",
        0x00D3: "Oacute",
        0x00D4: "Ocircumflex",
        0x00D5: "Otilde",
        0x00D6: "Odiaeresis",
        0x00D7: "multiply",
        0x00D8: "Oslash",
        0x00D8: "Ooblique",
        0x00D9: "Ugrave",
        0x00DA: "Uacute",
        0x00DB: "Ucircumflex",
        0x00DC: "Udiaeresis",
        0x00DD: "Yacute",
        0x00DE: "THORN",
        0x00DE: "Thorn",
        0x00DF: "ssharp",
        0x00E0: "agrave",
        0x00E1: "aacute",
        0x00E2: "acircumflex",
        0x00E3: "atilde",
        0x00E4: "adiaeresis",
        0x00E5: "aring",
        0x00E6: "ae",
        0x00E7: "ccedilla",
        0x00E8: "egrave",
        0x00E9: "eacute",
        0x00EA: "ecircumflex",
        0x00EB: "ediaeresis",
        0x00EC: "igrave",
        0x00ED: "iacute",
        0x00EE: "icircumflex",
        0x00EF: "idiaeresis",
        0x00F0: "eth",
        0x00F1: "ntilde",
        0x00F2: "ograve",
        0x00F3: "oacute",
        0x00F4: "ocircumflex",
        0x00F5: "otilde",
        0x00F6: "odiaeresis",
        0x00F7: "division",
        0x00F8: "oslash",
        0x00F8: "ooblique",
        0x00F9: "ugrave",
        0x00FA: "uacute",
        0x00FB: "ucircumflex",
        0x00FC: "udiaeresis",
        0x00FD: "yacute",
        0x00FE: "thorn",
        0x00FF: "ydiaeresis",
        0x01A1: "Aogonek",
        0x01A2: "breve",
        0x01A3: "Lstroke",
        0x01A5: "Lcaron",
        0x01A6: "Sacute",
        0x01A9: "Scaron",
        0x01AA: "Scedilla",
        0x01AB: "Tcaron",
        0x01AC: "Zacute",
        0x01AE: "Zcaron",
        0x01AF: "Zabovedot",
        0x01B1: "aogonek",
        0x01B2: "ogonek",
        0x01B3: "lstroke",
        0x01B5: "lcaron",
        0x01B6: "sacute",
        0x01B7: "caron",
        0x01B9: "scaron",
        0x01BA: "scedilla",
        0x01BB: "tcaron",
        0x01BC: "zacute",
        0x01BD: "doubleacute",
        0x01BE: "zcaron",
        0x01BF: "zabovedot",
        0x01C0: "Racute",
        0x01C3: "Abreve",
        0x01C5: "Lacute",
        0x01C6: "Cacute",
        0x01C8: "Ccaron",
        0x01CA: "Eogonek",
        0x01CC: "Ecaron",
        0x01CF: "Dcaron",
        0x01D0: "Dstroke",
        0x01D1: "Nacute",
        0x01D2: "Ncaron",
        0x01D5: "Odoubleacute",
        0x01D8: "Rcaron",
        0x01D9: "Uring",
        0x01DB: "Udoubleacute",
        0x01DE: "Tcedilla",
        0x01E0: "racute",
        0x01E3: "abreve",
        0x01E5: "lacute",
        0x01E6: "cacute",
        0x01E8: "ccaron",
        0x01EA: "eogonek",
        0x01EC: "ecaron",
        0x01EF: "dcaron",
        0x01F0: "dstroke",
        0x01F1: "nacute",
        0x01F2: "ncaron",
        0x01F5: "odoubleacute",
        0x01F8: "rcaron",
        0x01F9: "uring",
        0x01FB: "udoubleacute",
        0x01FE: "tcedilla",
        0x01FF: "abovedot",
        0x02A1: "Hstroke",
        0x02A6: "Hcircumflex",
        0x02A9: "Iabovedot",
        0x02AB: "Gbreve",
        0x02AC: "Jcircumflex",
        0x02B1: "hstroke",
        0x02B6: "hcircumflex",
        0x02B9: "idotless",
        0x02BB: "gbreve",
        0x02BC: "jcircumflex",
        0x02C5: "Cabovedot",
        0x02C6: "Ccircumflex",
        0x02D5: "Gabovedot",
        0x02D8: "Gcircumflex",
        0x02DD: "Ubreve",
        0x02DE: "Scircumflex",
        0x02E5: "cabovedot",
        0x02E6: "ccircumflex",
        0x02F5: "gabovedot",
        0x02F8: "gcircumflex",
        0x02FD: "ubreve",
        0x02FE: "scircumflex",
        0x03A2: "kra",
        0x03A2: "kappa",
        0x03A3: "Rcedilla",
        0x03A5: "Itilde",
        0x03A6: "Lcedilla",
        0x03AA: "Emacron",
        0x03AB: "Gcedilla",
        0x03AC: "Tslash",
        0x03B3: "rcedilla",
        0x03B5: "itilde",
        0x03B6: "lcedilla",
        0x03BA: "emacron",
        0x03BB: "gcedilla",
        0x03BC: "tslash",
        0x03BD: "ENG",
        0x03BF: "eng",
        0x03C0: "Amacron",
        0x03C7: "Iogonek",
        0x03CC: "Eabovedot",
        0x03CF: "Imacron",
        0x03D1: "Ncedilla",
        0x03D2: "Omacron",
        0x03D3: "Kcedilla",
        0x03D9: "Uogonek",
        0x03DD: "Utilde",
        0x03DE: "Umacron",
        0x03E0: "amacron",
        0x03E7: "iogonek",
        0x03EC: "eabovedot",
        0x03EF: "imacron",
        0x03F1: "ncedilla",
        0x03F2: "omacron",
        0x03F3: "kcedilla",
        0x03F9: "uogonek",
        0x03FD: "utilde",
        0x03FE: "umacron",
        0x1000174: "Wcircumflex",
        0x1000175: "wcircumflex",
        0x1000176: "Ycircumflex",
        0x1000177: "ycircumflex",
        0x1001E02: "Babovedot",
        0x1001E03: "babovedot",
        0x1001E0A: "Dabovedot",
        0x1001E0B: "dabovedot",
        0x1001E1E: "Fabovedot",
        0x1001E1F: "fabovedot",
        0x1001E40: "Mabovedot",
        0x1001E41: "mabovedot",
        0x1001E56: "Pabovedot",
        0x1001E57: "pabovedot",
        0x1001E60: "Sabovedot",
        0x1001E61: "sabovedot",
        0x1001E6A: "Tabovedot",
        0x1001E6B: "tabovedot",
        0x1001E80: "Wgrave",
        0x1001E81: "wgrave",
        0x1001E82: "Wacute",
        0x1001E83: "wacute",
        0x1001E84: "Wdiaeresis",
        0x1001E85: "wdiaeresis",
        0x1001EF2: "Ygrave",
        0x1001EF3: "ygrave",
        0x13BC: "OE",
        0x13BD: "oe",
        0x13BE: "Ydiaeresis",
        0x047E: "overline",
        0x04A1: "kana_fullstop",
        0x04A2: "kana_openingbracket",
        0x04A3: "kana_closingbracket",
        0x04A4: "kana_comma",
        0x04A5: "kana_conjunctive",
        0x04A5: "kana_middledot",
        0x04A6: "kana_WO",
        0x04A7: "kana_a",
        0x04A8: "kana_i",
        0x04A9: "kana_u",
        0x04AA: "kana_e",
        0x04AB: "kana_o",
        0x04AC: "kana_ya",
        0x04AD: "kana_yu",
        0x04AE: "kana_yo",
        0x04AF: "kana_tsu",
        0x04AF: "kana_tu",
        0x04B0: "prolongedsound",
        0x04B1: "kana_A",
        0x04B2: "kana_I",
        0x04B3: "kana_U",
        0x04B4: "kana_E",
        0x04B5: "kana_O",
        0x04B6: "kana_KA",
        0x04B7: "kana_KI",
        0x04B8: "kana_KU",
        0x04B9: "kana_KE",
        0x04BA: "kana_KO",
        0x04BB: "kana_SA",
        0x04BC: "kana_SHI",
        0x04BD: "kana_SU",
        0x04BE: "kana_SE",
        0x04BF: "kana_SO",
        0x04C0: "kana_TA",
        0x04C1: "kana_CHI",
        0x04C1: "kana_TI",
        0x04C2: "kana_TSU",
        0x04C2: "kana_TU",
        0x04C3: "kana_TE",
        0x04C4: "kana_TO",
        0x04C5: "kana_NA",
        0x04C6: "kana_NI",
        0x04C7: "kana_NU",
        0x04C8: "kana_NE",
        0x04C9: "kana_NO",
        0x04CA: "kana_HA",
        0x04CB: "kana_HI",
        0x04CC: "kana_FU",
        0x04CC: "kana_HU",
        0x04CD: "kana_HE",
        0x04CE: "kana_HO",
        0x04CF: "kana_MA",
        0x04D0: "kana_MI",
        0x04D1: "kana_MU",
        0x04D2: "kana_ME",
        0x04D3: "kana_MO",
        0x04D4: "kana_YA",
        0x04D5: "kana_YU",
        0x04D6: "kana_YO",
        0x04D7: "kana_RA",
        0x04D8: "kana_RI",
        0x04D9: "kana_RU",
        0x04DA: "kana_RE",
        0x04DB: "kana_RO",
        0x04DC: "kana_WA",
        0x04DD: "kana_N",
        0x04DE: "voicedsound",
        0x04DF: "semivoicedsound",
        0xFF7E: "kana_switch",
        0x10006F0: "Farsi_0",
        0x10006F1: "Farsi_1",
        0x10006F2: "Farsi_2",
        0x10006F3: "Farsi_3",
        0x10006F4: "Farsi_4",
        0x10006F5: "Farsi_5",
        0x10006F6: "Farsi_6",
        0x10006F7: "Farsi_7",
        0x10006F8: "Farsi_8",
        0x10006F9: "Farsi_9",
        0x100066A: "Arabic_percent",
        0x1000670: "Arabic_superscript_alef",
        0x1000679: "Arabic_tteh",
        0x100067E: "Arabic_peh",
        0x1000686: "Arabic_tcheh",
        0x1000688: "Arabic_ddal",
        0x1000691: "Arabic_rreh",
        0x05AC: "Arabic_comma",
        0x10006D4: "Arabic_fullstop",
        0x1000660: "Arabic_0",
        0x1000661: "Arabic_1",
        0x1000662: "Arabic_2",
        0x1000663: "Arabic_3",
        0x1000664: "Arabic_4",
        0x1000665: "Arabic_5",
        0x1000666: "Arabic_6",
        0x1000667: "Arabic_7",
        0x1000668: "Arabic_8",
        0x1000669: "Arabic_9",
        0x05BB: "Arabic_semicolon",
        0x05BF: "Arabic_question_mark",
        0x05C1: "Arabic_hamza",
        0x05C2: "Arabic_maddaonalef",
        0x05C3: "Arabic_hamzaonalef",
        0x05C4: "Arabic_hamzaonwaw",
        0x05C5: "Arabic_hamzaunderalef",
        0x05C6: "Arabic_hamzaonyeh",
        0x05C7: "Arabic_alef",
        0x05C8: "Arabic_beh",
        0x05C9: "Arabic_tehmarbuta",
        0x05CA: "Arabic_teh",
        0x05CB: "Arabic_theh",
        0x05CC: "Arabic_jeem",
        0x05CD: "Arabic_hah",
        0x05CE: "Arabic_khah",
        0x05CF: "Arabic_dal",
        0x05D0: "Arabic_thal",
        0x05D1: "Arabic_ra",
        0x05D2: "Arabic_zain",
        0x05D3: "Arabic_seen",
        0x05D4: "Arabic_sheen",
        0x05D5: "Arabic_sad",
        0x05D6: "Arabic_dad",
        0x05D7: "Arabic_tah",
        0x05D8: "Arabic_zah",
        0x05D9: "Arabic_ain",
        0x05DA: "Arabic_ghain",
        0x05E0: "Arabic_tatweel",
        0x05E1: "Arabic_feh",
        0x05E2: "Arabic_qaf",
        0x05E3: "Arabic_kaf",
        0x05E4: "Arabic_lam",
        0x05E5: "Arabic_meem",
        0x05E6: "Arabic_noon",
        0x05E7: "Arabic_ha",
        0x05E7: "Arabic_heh",
        0x05E8: "Arabic_waw",
        0x05E9: "Arabic_alefmaksura",
        0x05EA: "Arabic_yeh",
        0x05EB: "Arabic_fathatan",
        0x05EC: "Arabic_dammatan",
        0x05ED: "Arabic_kasratan",
        0x05EE: "Arabic_fatha",
        0x05EF: "Arabic_damma",
        0x05F0: "Arabic_kasra",
        0x05F1: "Arabic_shadda",
        0x05F2: "Arabic_sukun",
        0x1000653: "Arabic_madda_above",
        0x1000654: "Arabic_hamza_above",
        0x1000655: "Arabic_hamza_below",
        0x1000698: "Arabic_jeh",
        0x10006A4: "Arabic_veh",
        0x10006A9: "Arabic_keheh",
        0x10006AF: "Arabic_gaf",
        0x10006BA: "Arabic_noon_ghunna",
        0x10006BE: "Arabic_heh_doachashmee",
        0x10006CC: "Farsi_yeh",
        0x10006CC: "Arabic_farsi_yeh",
        0x10006D2: "Arabic_yeh_baree",
        0x10006C1: "Arabic_heh_goal",
        0xFF7E: "Arabic_switch",
        0x1000492: "Cyrillic_GHE_bar",
        0x1000493: "Cyrillic_ghe_bar",
        0x1000496: "Cyrillic_ZHE_descender",
        0x1000497: "Cyrillic_zhe_descender",
        0x100049A: "Cyrillic_KA_descender",
        0x100049B: "Cyrillic_ka_descender",
        0x100049C: "Cyrillic_KA_vertstroke",
        0x100049D: "Cyrillic_ka_vertstroke",
        0x10004A2: "Cyrillic_EN_descender",
        0x10004A3: "Cyrillic_en_descender",
        0x10004AE: "Cyrillic_U_straight",
        0x10004AF: "Cyrillic_u_straight",
        0x10004B0: "Cyrillic_U_straight_bar",
        0x10004B1: "Cyrillic_u_straight_bar",
        0x10004B2: "Cyrillic_HA_descender",
        0x10004B3: "Cyrillic_ha_descender",
        0x10004B6: "Cyrillic_CHE_descender",
        0x10004B7: "Cyrillic_che_descender",
        0x10004B8: "Cyrillic_CHE_vertstroke",
        0x10004B9: "Cyrillic_che_vertstroke",
        0x10004BA: "Cyrillic_SHHA",
        0x10004BB: "Cyrillic_shha",
        0x10004D8: "Cyrillic_SCHWA",
        0x10004D9: "Cyrillic_schwa",
        0x10004E2: "Cyrillic_I_macron",
        0x10004E3: "Cyrillic_i_macron",
        0x10004E8: "Cyrillic_O_bar",
        0x10004E9: "Cyrillic_o_bar",
        0x10004EE: "Cyrillic_U_macron",
        0x10004EF: "Cyrillic_u_macron",
        0x06A1: "Serbian_dje",
        0x06A2: "Macedonia_gje",
        0x06A3: "Cyrillic_io",
        0x06A4: "Ukrainian_ie",
        0x06A4: "Ukranian_je",
        0x06A5: "Macedonia_dse",
        0x06A6: "Ukrainian_i",
        0x06A6: "Ukranian_i",
        0x06A7: "Ukrainian_yi",
        0x06A7: "Ukranian_yi",
        0x06A8: "Cyrillic_je",
        0x06A8: "Serbian_je",
        0x06A9: "Cyrillic_lje",
        0x06A9: "Serbian_lje",
        0x06AA: "Cyrillic_nje",
        0x06AA: "Serbian_nje",
        0x06AB: "Serbian_tshe",
        0x06AC: "Macedonia_kje",
        0x06AD: "Ukrainian_ghe_with_upturn",
        0x06AE: "Byelorussian_shortu",
        0x06AF: "Cyrillic_dzhe",
        0x06AF: "Serbian_dze",
        0x06B0: "numerosign",
        0x06B1: "Serbian_DJE",
        0x06B2: "Macedonia_GJE",
        0x06B3: "Cyrillic_IO",
        0x06B4: "Ukrainian_IE",
        0x06B4: "Ukranian_JE",
        0x06B5: "Macedonia_DSE",
        0x06B6: "Ukrainian_I",
        0x06B6: "Ukranian_I",
        0x06B7: "Ukrainian_YI",
        0x06B7: "Ukranian_YI",
        0x06B8: "Cyrillic_JE",
        0x06B8: "Serbian_JE",
        0x06B9: "Cyrillic_LJE",
        0x06B9: "Serbian_LJE",
        0x06BA: "Cyrillic_NJE",
        0x06BA: "Serbian_NJE",
        0x06BB: "Serbian_TSHE",
        0x06BC: "Macedonia_KJE",
        0x06BD: "Ukrainian_GHE_WITH_UPTURN",
        0x06BE: "Byelorussian_SHORTU",
        0x06BF: "Cyrillic_DZHE",
        0x06BF: "Serbian_DZE",
        0x06C0: "Cyrillic_yu",
        0x06C1: "Cyrillic_a",
        0x06C2: "Cyrillic_be",
        0x06C3: "Cyrillic_tse",
        0x06C4: "Cyrillic_de",
        0x06C5: "Cyrillic_ie",
        0x06C6: "Cyrillic_ef",
        0x06C7: "Cyrillic_ghe",
        0x06C8: "Cyrillic_ha",
        0x06C9: "Cyrillic_i",
        0x06CA: "Cyrillic_shorti",
        0x06CB: "Cyrillic_ka",
        0x06CC: "Cyrillic_el",
        0x06CD: "Cyrillic_em",
        0x06CE: "Cyrillic_en",
        0x06CF: "Cyrillic_o",
        0x06D0: "Cyrillic_pe",
        0x06D1: "Cyrillic_ya",
        0x06D2: "Cyrillic_er",
        0x06D3: "Cyrillic_es",
        0x06D4: "Cyrillic_te",
        0x06D5: "Cyrillic_u",
        0x06D6: "Cyrillic_zhe",
        0x06D7: "Cyrillic_ve",
        0x06D8: "Cyrillic_softsign",
        0x06D9: "Cyrillic_yeru",
        0x06DA: "Cyrillic_ze",
        0x06DB: "Cyrillic_sha",
        0x06DC: "Cyrillic_e",
        0x06DD: "Cyrillic_shcha",
        0x06DE: "Cyrillic_che",
        0x06DF: "Cyrillic_hardsign",
        0x06E0: "Cyrillic_YU",
        0x06E1: "Cyrillic_A",
        0x06E2: "Cyrillic_BE",
        0x06E3: "Cyrillic_TSE",
        0x06E4: "Cyrillic_DE",
        0x06E5: "Cyrillic_IE",
        0x06E6: "Cyrillic_EF",
        0x06E7: "Cyrillic_GHE",
        0x06E8: "Cyrillic_HA",
        0x06E9: "Cyrillic_I",
        0x06EA: "Cyrillic_SHORTI",
        0x06EB: "Cyrillic_KA",
        0x06EC: "Cyrillic_EL",
        0x06ED: "Cyrillic_EM",
        0x06EE: "Cyrillic_EN",
        0x06EF: "Cyrillic_O",
        0x06F0: "Cyrillic_PE",
        0x06F1: "Cyrillic_YA",
        0x06F2: "Cyrillic_ER",
        0x06F3: "Cyrillic_ES",
        0x06F4: "Cyrillic_TE",
        0x06F5: "Cyrillic_U",
        0x06F6: "Cyrillic_ZHE",
        0x06F7: "Cyrillic_VE",
        0x06F8: "Cyrillic_SOFTSIGN",
        0x06F9: "Cyrillic_YERU",
        0x06FA: "Cyrillic_ZE",
        0x06FB: "Cyrillic_SHA",
        0x06FC: "Cyrillic_E",
        0x06FD: "Cyrillic_SHCHA",
        0x06FE: "Cyrillic_CHE",
        0x06FF: "Cyrillic_HARDSIGN",
        0x07A1: "Greek_ALPHAaccent",
        0x07A2: "Greek_EPSILONaccent",
        0x07A3: "Greek_ETAaccent",
        0x07A4: "Greek_IOTAaccent",
        0x07A5: "Greek_IOTAdieresis",
        0x07A5: "Greek_IOTAdiaeresis",
        0x07A7: "Greek_OMICRONaccent",
        0x07A8: "Greek_UPSILONaccent",
        0x07A9: "Greek_UPSILONdieresis",
        0x07AB: "Greek_OMEGAaccent",
        0x07AE: "Greek_accentdieresis",
        0x07AF: "Greek_horizbar",
        0x07B1: "Greek_alphaaccent",
        0x07B2: "Greek_epsilonaccent",
        0x07B3: "Greek_etaaccent",
        0x07B4: "Greek_iotaaccent",
        0x07B5: "Greek_iotadieresis",
        0x07B6: "Greek_iotaaccentdieresis",
        0x07B7: "Greek_omicronaccent",
        0x07B8: "Greek_upsilonaccent",
        0x07B9: "Greek_upsilondieresis",
        0x07BA: "Greek_upsilonaccentdieresis",
        0x07BB: "Greek_omegaaccent",
        0x07C1: "Greek_ALPHA",
        0x07C2: "Greek_BETA",
        0x07C3: "Greek_GAMMA",
        0x07C4: "Greek_DELTA",
        0x07C5: "Greek_EPSILON",
        0x07C6: "Greek_ZETA",
        0x07C7: "Greek_ETA",
        0x07C8: "Greek_THETA",
        0x07C9: "Greek_IOTA",
        0x07CA: "Greek_KAPPA",
        0x07CB: "Greek_LAMDA",
        0x07CB: "Greek_LAMBDA",
        0x07CC: "Greek_MU",
        0x07CD: "Greek_NU",
        0x07CE: "Greek_XI",
        0x07CF: "Greek_OMICRON",
        0x07D0: "Greek_PI",
        0x07D1: "Greek_RHO",
        0x07D2: "Greek_SIGMA",
        0x07D4: "Greek_TAU",
        0x07D5: "Greek_UPSILON",
        0x07D6: "Greek_PHI",
        0x07D7: "Greek_CHI",
        0x07D8: "Greek_PSI",
        0x07D9: "Greek_OMEGA",
        0x07E1: "Greek_alpha",
        0x07E2: "Greek_beta",
        0x07E3: "Greek_gamma",
        0x07E4: "Greek_delta",
        0x07E5: "Greek_epsilon",
        0x07E6: "Greek_zeta",
        0x07E7: "Greek_eta",
        0x07E8: "Greek_theta",
        0x07E9: "Greek_iota",
        0x07EA: "Greek_kappa",
        0x07EB: "Greek_lamda",
        0x07EB: "Greek_lambda",
        0x07EC: "Greek_mu",
        0x07ED: "Greek_nu",
        0x07EE: "Greek_xi",
        0x07EF: "Greek_omicron",
        0x07F0: "Greek_pi",
        0x07F1: "Greek_rho",
        0x07F2: "Greek_sigma",
        0x07F3: "Greek_finalsmallsigma",
        0x07F4: "Greek_tau",
        0x07F5: "Greek_upsilon",
        0x07F6: "Greek_phi",
        0x07F7: "Greek_chi",
        0x07F8: "Greek_psi",
        0x07F9: "Greek_omega",
        0xFF7E: "Greek_switch",
        0x08A1: "leftradical",
        0x08A2: "topleftradical",
        0x08A3: "horizconnector",
        0x08A4: "topintegral",
        0x08A5: "botintegral",
        0x08A6: "vertconnector",
        0x08A7: "topleftsqbracket",
        0x08A8: "botleftsqbracket",
        0x08A9: "toprightsqbracket",
        0x08AA: "botrightsqbracket",
        0x08AB: "topleftparens",
        0x08AC: "botleftparens",
        0x08AD: "toprightparens",
        0x08AE: "botrightparens",
        0x08AF: "leftmiddlecurlybrace",
        0x08B0: "rightmiddlecurlybrace",
        0x08B1: "topleftsummation",
        0x08B2: "botleftsummation",
        0x08B3: "topvertsummationconnector",
        0x08B4: "botvertsummationconnector",
        0x08B5: "toprightsummation",
        0x08B6: "botrightsummation",
        0x08B7: "rightmiddlesummation",
        0x08BC: "lessthanequal",
        0x08BD: "notequal",
        0x08BE: "greaterthanequal",
        0x08BF: "integral",
        0x08C0: "therefore",
        0x08C1: "variation",
        0x08C2: "infinity",
        0x08C5: "nabla",
        0x08C8: "approximate",
        0x08C9: "similarequal",
        0x08CD: "ifonlyif",
        0x08CE: "implies",
        0x08CF: "identical",
        0x08D6: "radical",
        0x08DA: "includedin",
        0x08DB: "includes",
        0x08DC: "intersection",
        0x08DD: "union",
        0x08DE: "logicaland",
        0x08DF: "logicalor",
        0x08EF: "partialderivative",
        0x08F6: "function",
        0x08FB: "leftarrow",
        0x08FC: "uparrow",
        0x08FD: "rightarrow",
        0x08FE: "downarrow",
        0x09DF: "blank",
        0x09E0: "soliddiamond",
        0x09E1: "checkerboard",
        0x09E2: "ht",
        0x09E3: "ff",
        0x09E4: "cr",
        0x09E5: "lf",
        0x09E8: "nl",
        0x09E9: "vt",
        0x09EA: "lowrightcorner",
        0x09EB: "uprightcorner",
        0x09EC: "upleftcorner",
        0x09ED: "lowleftcorner",
        0x09EE: "crossinglines",
        0x09EF: "horizlinescan1",
        0x09F0: "horizlinescan3",
        0x09F1: "horizlinescan5",
        0x09F2: "horizlinescan7",
        0x09F3: "horizlinescan9",
        0x09F4: "leftt",
        0x09F5: "rightt",
        0x09F6: "bott",
        0x09F7: "topt",
        0x09F8: "vertbar",
        0x0AA1: "emspace",
        0x0AA2: "enspace",
        0x0AA3: "em3space",
        0x0AA4: "em4space",
        0x0AA5: "digitspace",
        0x0AA6: "punctspace",
        0x0AA7: "thinspace",
        0x0AA8: "hairspace",
        0x0AA9: "emdash",
        0x0AAA: "endash",
        0x0AAC: "signifblank",
        0x0AAE: "ellipsis",
        0x0AAF: "doubbaselinedot",
        0x0AB0: "onethird",
        0x0AB1: "twothirds",
        0x0AB2: "onefifth",
        0x0AB3: "twofifths",
        0x0AB4: "threefifths",
        0x0AB5: "fourfifths",
        0x0AB6: "onesixth",
        0x0AB7: "fivesixths",
        0x0AB8: "careof",
        0x0ABB: "figdash",
        0x0ABC: "leftanglebracket",
        0x0ABD: "decimalpoint",
        0x0ABE: "rightanglebracket",
        0x0ABF: "marker",
        0x0AC3: "oneeighth",
        0x0AC4: "threeeighths",
        0x0AC5: "fiveeighths",
        0x0AC6: "seveneighths",
        0x0AC9: "trademark",
        0x0ACA: "signaturemark",
        0x0ACB: "trademarkincircle",
        0x0ACC: "leftopentriangle",
        0x0ACD: "rightopentriangle",
        0x0ACE: "emopencircle",
        0x0ACF: "emopenrectangle",
        0x0AD0: "leftsinglequotemark",
        0x0AD1: "rightsinglequotemark",
        0x0AD2: "leftdoublequotemark",
        0x0AD3: "rightdoublequotemark",
        0x0AD4: "prescription",
        0x0AD5: "permille",
        0x0AD6: "minutes",
        0x0AD7: "seconds",
        0x0AD9: "latincross",
        0x0ADA: "hexagram",
        0x0ADB: "filledrectbullet",
        0x0ADC: "filledlefttribullet",
        0x0ADD: "filledrighttribullet",
        0x0ADE: "emfilledcircle",
        0x0ADF: "emfilledrect",
        0x0AE0: "enopencircbullet",
        0x0AE1: "enopensquarebullet",
        0x0AE2: "openrectbullet",
        0x0AE3: "opentribulletup",
        0x0AE4: "opentribulletdown",
        0x0AE5: "openstar",
        0x0AE6: "enfilledcircbullet",
        0x0AE7: "enfilledsqbullet",
        0x0AE8: "filledtribulletup",
        0x0AE9: "filledtribulletdown",
        0x0AEA: "leftpointer",
        0x0AEB: "rightpointer",
        0x0AEC: "club",
        0x0AED: "diamond",
        0x0AEE: "heart",
        0x0AF0: "maltesecross",
        0x0AF1: "dagger",
        0x0AF2: "doubledagger",
        0x0AF3: "checkmark",
        0x0AF4: "ballotcross",
        0x0AF5: "musicalsharp",
        0x0AF6: "musicalflat",
        0x0AF7: "malesymbol",
        0x0AF8: "femalesymbol",
        0x0AF9: "telephone",
        0x0AFA: "telephonerecorder",
        0x0AFB: "phonographcopyright",
        0x0AFC: "caret",
        0x0AFD: "singlelowquotemark",
        0x0AFE: "doublelowquotemark",
        0x0AFF: "cursor",
        0x0BA3: "leftcaret",
        0x0BA6: "rightcaret",
        0x0BA8: "downcaret",
        0x0BA9: "upcaret",
        0x0BC0: "overbar",
        0x0BC2: "downtack",
        0x0BC3: "upshoe",
        0x0BC4: "downstile",
        0x0BC6: "underbar",
        0x0BCA: "jot",
        0x0BCC: "quad",
        0x0BCE: "uptack",
        0x0BCF: "circle",
        0x0BD3: "upstile",
        0x0BD6: "downshoe",
        0x0BD8: "rightshoe",
        0x0BDA: "leftshoe",
        0x0BDC: "lefttack",
        0x0BFC: "righttack",
        0x0CDF: "hebrew_doublelowline",
        0x0CE0: "hebrew_aleph",
        0x0CE1: "hebrew_bet",
        0x0CE1: "hebrew_beth",
        0x0CE2: "hebrew_gimel",
        0x0CE2: "hebrew_gimmel",
        0x0CE3: "hebrew_dalet",
        0x0CE3: "hebrew_daleth",
        0x0CE4: "hebrew_he",
        0x0CE5: "hebrew_waw",
        0x0CE6: "hebrew_zain",
        0x0CE6: "hebrew_zayin",
        0x0CE7: "hebrew_chet",
        0x0CE7: "hebrew_het",
        0x0CE8: "hebrew_tet",
        0x0CE8: "hebrew_teth",
        0x0CE9: "hebrew_yod",
        0x0CEA: "hebrew_finalkaph",
        0x0CEB: "hebrew_kaph",
        0x0CEC: "hebrew_lamed",
        0x0CED: "hebrew_finalmem",
        0x0CEE: "hebrew_mem",
        0x0CEF: "hebrew_finalnun",
        0x0CF0: "hebrew_nun",
        0x0CF1: "hebrew_samech",
        0x0CF1: "hebrew_samekh",
        0x0CF2: "hebrew_ayin",
        0x0CF3: "hebrew_finalpe",
        0x0CF4: "hebrew_pe",
        0x0CF5: "hebrew_finalzade",
        0x0CF5: "hebrew_finalzadi",
        0x0CF6: "hebrew_zade",
        0x0CF6: "hebrew_zadi",
        0x0CF7: "hebrew_qoph",
        0x0CF7: "hebrew_kuf",
        0x0CF8: "hebrew_resh",
        0x0CF9: "hebrew_shin",
        0x0CFA: "hebrew_taw",
        0x0CFA: "hebrew_taf",
        0xFF7E: "Hebrew_switch",
        0x0DA1: "Thai_kokai",
        0x0DA2: "Thai_khokhai",
        0x0DA3: "Thai_khokhuat",
        0x0DA4: "Thai_khokhwai",
        0x0DA5: "Thai_khokhon",
        0x0DA6: "Thai_khorakhang",
        0x0DA7: "Thai_ngongu",
        0x0DA8: "Thai_chochan",
        0x0DA9: "Thai_choching",
        0x0DAA: "Thai_chochang",
        0x0DAB: "Thai_soso",
        0x0DAC: "Thai_chochoe",
        0x0DAD: "Thai_yoying",
        0x0DAE: "Thai_dochada",
        0x0DAF: "Thai_topatak",
        0x0DB0: "Thai_thothan",
        0x0DB1: "Thai_thonangmontho",
        0x0DB2: "Thai_thophuthao",
        0x0DB3: "Thai_nonen",
        0x0DB4: "Thai_dodek",
        0x0DB5: "Thai_totao",
        0x0DB6: "Thai_thothung",
        0x0DB7: "Thai_thothahan",
        0x0DB8: "Thai_thothong",
        0x0DB9: "Thai_nonu",
        0x0DBA: "Thai_bobaimai",
        0x0DBB: "Thai_popla",
        0x0DBC: "Thai_phophung",
        0x0DBD: "Thai_fofa",
        0x0DBE: "Thai_phophan",
        0x0DBF: "Thai_fofan",
        0x0DC0: "Thai_phosamphao",
        0x0DC1: "Thai_moma",
        0x0DC2: "Thai_yoyak",
        0x0DC3: "Thai_rorua",
        0x0DC4: "Thai_ru",
        0x0DC5: "Thai_loling",
        0x0DC6: "Thai_lu",
        0x0DC7: "Thai_wowaen",
        0x0DC8: "Thai_sosala",
        0x0DC9: "Thai_sorusi",
        0x0DCA: "Thai_sosua",
        0x0DCB: "Thai_hohip",
        0x0DCC: "Thai_lochula",
        0x0DCD: "Thai_oang",
        0x0DCE: "Thai_honokhuk",
        0x0DCF: "Thai_paiyannoi",
        0x0DD0: "Thai_saraa",
        0x0DD1: "Thai_maihanakat",
        0x0DD2: "Thai_saraaa",
        0x0DD3: "Thai_saraam",
        0x0DD4: "Thai_sarai",
        0x0DD5: "Thai_saraii",
        0x0DD6: "Thai_saraue",
        0x0DD7: "Thai_sarauee",
        0x0DD8: "Thai_sarau",
        0x0DD9: "Thai_sarauu",
        0x0DDA: "Thai_phinthu",
        0x0DDE: "Thai_maihanakat_maitho",
        0x0DDF: "Thai_baht",
        0x0DE0: "Thai_sarae",
        0x0DE1: "Thai_saraae",
        0x0DE2: "Thai_sarao",
        0x0DE3: "Thai_saraaimaimuan",
        0x0DE4: "Thai_saraaimaimalai",
        0x0DE5: "Thai_lakkhangyao",
        0x0DE6: "Thai_maiyamok",
        0x0DE7: "Thai_maitaikhu",
        0x0DE8: "Thai_maiek",
        0x0DE9: "Thai_maitho",
        0x0DEA: "Thai_maitri",
        0x0DEB: "Thai_maichattawa",
        0x0DEC: "Thai_thanthakhat",
        0x0DED: "Thai_nikhahit",
        0x0DF0: "Thai_leksun",
        0x0DF1: "Thai_leknung",
        0x0DF2: "Thai_leksong",
        0x0DF3: "Thai_leksam",
        0x0DF4: "Thai_leksi",
        0x0DF5: "Thai_lekha",
        0x0DF6: "Thai_lekhok",
        0x0DF7: "Thai_lekchet",
        0x0DF8: "Thai_lekpaet",
        0x0DF9: "Thai_lekkao",
        0xFF31: "Hangul",
        0xFF32: "Hangul_Start",
        0xFF33: "Hangul_End",
        0xFF34: "Hangul_Hanja",
        0xFF35: "Hangul_Jamo",
        0xFF36: "Hangul_Romaja",
        0xFF37: "Hangul_Codeinput",
        0xFF38: "Hangul_Jeonja",
        0xFF39: "Hangul_Banja",
        0xFF3A: "Hangul_PreHanja",
        0xFF3B: "Hangul_PostHanja",
        0xFF3C: "Hangul_SingleCandidate",
        0xFF3D: "Hangul_MultipleCandidate",
        0xFF3E: "Hangul_PreviousCandidate",
        0xFF3F: "Hangul_Special",
        0xFF7E: "Hangul_switch",
        0x0EA1: "Hangul_Kiyeog",
        0x0EA2: "Hangul_SsangKiyeog",
        0x0EA3: "Hangul_KiyeogSios",
        0x0EA4: "Hangul_Nieun",
        0x0EA5: "Hangul_NieunJieuj",
        0x0EA6: "Hangul_NieunHieuh",
        0x0EA7: "Hangul_Dikeud",
        0x0EA8: "Hangul_SsangDikeud",
        0x0EA9: "Hangul_Rieul",
        0x0EAA: "Hangul_RieulKiyeog",
        0x0EAB: "Hangul_RieulMieum",
        0x0EAC: "Hangul_RieulPieub",
        0x0EAD: "Hangul_RieulSios",
        0x0EAE: "Hangul_RieulTieut",
        0x0EAF: "Hangul_RieulPhieuf",
        0x0EB0: "Hangul_RieulHieuh",
        0x0EB1: "Hangul_Mieum",
        0x0EB2: "Hangul_Pieub",
        0x0EB3: "Hangul_SsangPieub",
        0x0EB4: "Hangul_PieubSios",
        0x0EB5: "Hangul_Sios",
        0x0EB6: "Hangul_SsangSios",
        0x0EB7: "Hangul_Ieung",
        0x0EB8: "Hangul_Jieuj",
        0x0EB9: "Hangul_SsangJieuj",
        0x0EBA: "Hangul_Cieuc",
        0x0EBB: "Hangul_Khieuq",
        0x0EBC: "Hangul_Tieut",
        0x0EBD: "Hangul_Phieuf",
        0x0EBE: "Hangul_Hieuh",
        0x0EBF: "Hangul_A",
        0x0EC0: "Hangul_AE",
        0x0EC1: "Hangul_YA",
        0x0EC2: "Hangul_YAE",
        0x0EC3: "Hangul_EO",
        0x0EC4: "Hangul_E",
        0x0EC5: "Hangul_YEO",
        0x0EC6: "Hangul_YE",
        0x0EC7: "Hangul_O",
        0x0EC8: "Hangul_WA",
        0x0EC9: "Hangul_WAE",
        0x0ECA: "Hangul_OE",
        0x0ECB: "Hangul_YO",
        0x0ECC: "Hangul_U",
        0x0ECD: "Hangul_WEO",
        0x0ECE: "Hangul_WE",
        0x0ECF: "Hangul_WI",
        0x0ED0: "Hangul_YU",
        0x0ED1: "Hangul_EU",
        0x0ED2: "Hangul_YI",
        0x0ED3: "Hangul_I",
        0x0ED4: "Hangul_J_Kiyeog",
        0x0ED5: "Hangul_J_SsangKiyeog",
        0x0ED6: "Hangul_J_KiyeogSios",
        0x0ED7: "Hangul_J_Nieun",
        0x0ED8: "Hangul_J_NieunJieuj",
        0x0ED9: "Hangul_J_NieunHieuh",
        0x0EDA: "Hangul_J_Dikeud",
        0x0EDB: "Hangul_J_Rieul",
        0x0EDC: "Hangul_J_RieulKiyeog",
        0x0EDD: "Hangul_J_RieulMieum",
        0x0EDE: "Hangul_J_RieulPieub",
        0x0EDF: "Hangul_J_RieulSios",
        0x0EE0: "Hangul_J_RieulTieut",
        0x0EE1: "Hangul_J_RieulPhieuf",
        0x0EE2: "Hangul_J_RieulHieuh",
        0x0EE3: "Hangul_J_Mieum",
        0x0EE4: "Hangul_J_Pieub",
        0x0EE5: "Hangul_J_PieubSios",
        0x0EE6: "Hangul_J_Sios",
        0x0EE7: "Hangul_J_SsangSios",
        0x0EE8: "Hangul_J_Ieung",
        0x0EE9: "Hangul_J_Jieuj",
        0x0EEA: "Hangul_J_Cieuc",
        0x0EEB: "Hangul_J_Khieuq",
        0x0EEC: "Hangul_J_Tieut",
        0x0EED: "Hangul_J_Phieuf",
        0x0EEE: "Hangul_J_Hieuh",
        0x0EEF: "Hangul_RieulYeorinHieuh",
        0x0EF0: "Hangul_SunkyeongeumMieum",
        0x0EF1: "Hangul_SunkyeongeumPieub",
        0x0EF2: "Hangul_PanSios",
        0x0EF3: "Hangul_KkogjiDalrinIeung",
        0x0EF4: "Hangul_SunkyeongeumPhieuf",
        0x0EF5: "Hangul_YeorinHieuh",
        0x0EF6: "Hangul_AraeA",
        0x0EF7: "Hangul_AraeAE",
        0x0EF8: "Hangul_J_PanSios",
        0x0EF9: "Hangul_J_KkogjiDalrinIeung",
        0x0EFA: "Hangul_J_YeorinHieuh",
        0x0EFF: "Korean_Won",
        0x1000587: "Armenian_ligature_ew",
        0x1000589: "Armenian_full_stop",
        0x1000589: "Armenian_verjaket",
        0x100055D: "Armenian_separation_mark",
        0x100055D: "Armenian_but",
        0x100058A: "Armenian_hyphen",
        0x100058A: "Armenian_yentamna",
        0x100055C: "Armenian_exclam",
        0x100055C: "Armenian_amanak",
        0x100055B: "Armenian_accent",
        0x100055B: "Armenian_shesht",
        0x100055E: "Armenian_question",
        0x100055E: "Armenian_paruyk",
        0x1000531: "Armenian_AYB",
        0x1000561: "Armenian_ayb",
        0x1000532: "Armenian_BEN",
        0x1000562: "Armenian_ben",
        0x1000533: "Armenian_GIM",
        0x1000563: "Armenian_gim",
        0x1000534: "Armenian_DA",
        0x1000564: "Armenian_da",
        0x1000535: "Armenian_YECH",
        0x1000565: "Armenian_yech",
        0x1000536: "Armenian_ZA",
        0x1000566: "Armenian_za",
        0x1000537: "Armenian_E",
        0x1000567: "Armenian_e",
        0x1000538: "Armenian_AT",
        0x1000568: "Armenian_at",
        0x1000539: "Armenian_TO",
        0x1000569: "Armenian_to",
        0x100053A: "Armenian_ZHE",
        0x100056A: "Armenian_zhe",
        0x100053B: "Armenian_INI",
        0x100056B: "Armenian_ini",
        0x100053C: "Armenian_LYUN",
        0x100056C: "Armenian_lyun",
        0x100053D: "Armenian_KHE",
        0x100056D: "Armenian_khe",
        0x100053E: "Armenian_TSA",
        0x100056E: "Armenian_tsa",
        0x100053F: "Armenian_KEN",
        0x100056F: "Armenian_ken",
        0x1000540: "Armenian_HO",
        0x1000570: "Armenian_ho",
        0x1000541: "Armenian_DZA",
        0x1000571: "Armenian_dza",
        0x1000542: "Armenian_GHAT",
        0x1000572: "Armenian_ghat",
        0x1000543: "Armenian_TCHE",
        0x1000573: "Armenian_tche",
        0x1000544: "Armenian_MEN",
        0x1000574: "Armenian_men",
        0x1000545: "Armenian_HI",
        0x1000575: "Armenian_hi",
        0x1000546: "Armenian_NU",
        0x1000576: "Armenian_nu",
        0x1000547: "Armenian_SHA",
        0x1000577: "Armenian_sha",
        0x1000548: "Armenian_VO",
        0x1000578: "Armenian_vo",
        0x1000549: "Armenian_CHA",
        0x1000579: "Armenian_cha",
        0x100054A: "Armenian_PE",
        0x100057A: "Armenian_pe",
        0x100054B: "Armenian_JE",
        0x100057B: "Armenian_je",
        0x100054C: "Armenian_RA",
        0x100057C: "Armenian_ra",
        0x100054D: "Armenian_SE",
        0x100057D: "Armenian_se",
        0x100054E: "Armenian_VEV",
        0x100057E: "Armenian_vev",
        0x100054F: "Armenian_TYUN",
        0x100057F: "Armenian_tyun",
        0x1000550: "Armenian_RE",
        0x1000580: "Armenian_re",
        0x1000551: "Armenian_TSO",
        0x1000581: "Armenian_tso",
        0x1000552: "Armenian_VYUN",
        0x1000582: "Armenian_vyun",
        0x1000553: "Armenian_PYUR",
        0x1000583: "Armenian_pyur",
        0x1000554: "Armenian_KE",
        0x1000584: "Armenian_ke",
        0x1000555: "Armenian_O",
        0x1000585: "Armenian_o",
        0x1000556: "Armenian_FE",
        0x1000586: "Armenian_fe",
        0x100055A: "Armenian_apostrophe",
        0x10010D0: "Georgian_an",
        0x10010D1: "Georgian_ban",
        0x10010D2: "Georgian_gan",
        0x10010D3: "Georgian_don",
        0x10010D4: "Georgian_en",
        0x10010D5: "Georgian_vin",
        0x10010D6: "Georgian_zen",
        0x10010D7: "Georgian_tan",
        0x10010D8: "Georgian_in",
        0x10010D9: "Georgian_kan",
        0x10010DA: "Georgian_las",
        0x10010DB: "Georgian_man",
        0x10010DC: "Georgian_nar",
        0x10010DD: "Georgian_on",
        0x10010DE: "Georgian_par",
        0x10010DF: "Georgian_zhar",
        0x10010E0: "Georgian_rae",
        0x10010E1: "Georgian_san",
        0x10010E2: "Georgian_tar",
        0x10010E3: "Georgian_un",
        0x10010E4: "Georgian_phar",
        0x10010E5: "Georgian_khar",
        0x10010E6: "Georgian_ghan",
        0x10010E7: "Georgian_qar",
        0x10010E8: "Georgian_shin",
        0x10010E9: "Georgian_chin",
        0x10010EA: "Georgian_can",
        0x10010EB: "Georgian_jil",
        0x10010EC: "Georgian_cil",
        0x10010ED: "Georgian_char",
        0x10010EE: "Georgian_xan",
        0x10010EF: "Georgian_jhan",
        0x10010F0: "Georgian_hae",
        0x10010F1: "Georgian_he",
        0x10010F2: "Georgian_hie",
        0x10010F3: "Georgian_we",
        0x10010F4: "Georgian_har",
        0x10010F5: "Georgian_hoe",
        0x10010F6: "Georgian_fi",
        0x1001E8A: "Xabovedot",
        0x100012C: "Ibreve",
        0x10001B5: "Zstroke",
        0x10001E6: "Gcaron",
        0x10001D1: "Ocaron",
        0x100019F: "Obarred",
        0x1001E8B: "xabovedot",
        0x100012D: "ibreve",
        0x10001B6: "zstroke",
        0x10001E7: "gcaron",
        0x10001D2: "ocaron",
        0x1000275: "obarred",
        0x100018F: "SCHWA",
        0x1000259: "schwa",
        0x10001B7: "EZH",
        0x1000292: "ezh",
        0x1001E36: "Lbelowdot",
        0x1001E37: "lbelowdot",
        0x1001EA0: "Abelowdot",
        0x1001EA1: "abelowdot",
        0x1001EA2: "Ahook",
        0x1001EA3: "ahook",
        0x1001EA4: "Acircumflexacute",
        0x1001EA5: "acircumflexacute",
        0x1001EA6: "Acircumflexgrave",
        0x1001EA7: "acircumflexgrave",
        0x1001EA8: "Acircumflexhook",
        0x1001EA9: "acircumflexhook",
        0x1001EAA: "Acircumflextilde",
        0x1001EAB: "acircumflextilde",
        0x1001EAC: "Acircumflexbelowdot",
        0x1001EAD: "acircumflexbelowdot",
        0x1001EAE: "Abreveacute",
        0x1001EAF: "abreveacute",
        0x1001EB0: "Abrevegrave",
        0x1001EB1: "abrevegrave",
        0x1001EB2: "Abrevehook",
        0x1001EB3: "abrevehook",
        0x1001EB4: "Abrevetilde",
        0x1001EB5: "abrevetilde",
        0x1001EB6: "Abrevebelowdot",
        0x1001EB7: "abrevebelowdot",
        0x1001EB8: "Ebelowdot",
        0x1001EB9: "ebelowdot",
        0x1001EBA: "Ehook",
        0x1001EBB: "ehook",
        0x1001EBC: "Etilde",
        0x1001EBD: "etilde",
        0x1001EBE: "Ecircumflexacute",
        0x1001EBF: "ecircumflexacute",
        0x1001EC0: "Ecircumflexgrave",
        0x1001EC1: "ecircumflexgrave",
        0x1001EC2: "Ecircumflexhook",
        0x1001EC3: "ecircumflexhook",
        0x1001EC4: "Ecircumflextilde",
        0x1001EC5: "ecircumflextilde",
        0x1001EC6: "Ecircumflexbelowdot",
        0x1001EC7: "ecircumflexbelowdot",
        0x1001EC8: "Ihook",
        0x1001EC9: "ihook",
        0x1001ECA: "Ibelowdot",
        0x1001ECB: "ibelowdot",
        0x1001ECC: "Obelowdot",
        0x1001ECD: "obelowdot",
        0x1001ECE: "Ohook",
        0x1001ECF: "ohook",
        0x1001ED0: "Ocircumflexacute",
        0x1001ED1: "ocircumflexacute",
        0x1001ED2: "Ocircumflexgrave",
        0x1001ED3: "ocircumflexgrave",
        0x1001ED4: "Ocircumflexhook",
        0x1001ED5: "ocircumflexhook",
        0x1001ED6: "Ocircumflextilde",
        0x1001ED7: "ocircumflextilde",
        0x1001ED8: "Ocircumflexbelowdot",
        0x1001ED9: "ocircumflexbelowdot",
        0x1001EDA: "Ohornacute",
        0x1001EDB: "ohornacute",
        0x1001EDC: "Ohorngrave",
        0x1001EDD: "ohorngrave",
        0x1001EDE: "Ohornhook",
        0x1001EDF: "ohornhook",
        0x1001EE0: "Ohorntilde",
        0x1001EE1: "ohorntilde",
        0x1001EE2: "Ohornbelowdot",
        0x1001EE3: "ohornbelowdot",
        0x1001EE4: "Ubelowdot",
        0x1001EE5: "ubelowdot",
        0x1001EE6: "Uhook",
        0x1001EE7: "uhook",
        0x1001EE8: "Uhornacute",
        0x1001EE9: "uhornacute",
        0x1001EEA: "Uhorngrave",
        0x1001EEB: "uhorngrave",
        0x1001EEC: "Uhornhook",
        0x1001EED: "uhornhook",
        0x1001EEE: "Uhorntilde",
        0x1001EEF: "uhorntilde",
        0x1001EF0: "Uhornbelowdot",
        0x1001EF1: "uhornbelowdot",
        0x1001EF4: "Ybelowdot",
        0x1001EF5: "ybelowdot",
        0x1001EF6: "Yhook",
        0x1001EF7: "yhook",
        0x1001EF8: "Ytilde",
        0x1001EF9: "ytilde",
        0x10001A0: "Ohorn",
        0x10001A1: "ohorn",
        0x10001AF: "Uhorn",
        0x10001B0: "uhorn",
        0x10020A0: "EcuSign",
        0x10020A1: "ColonSign",
        0x10020A2: "CruzeiroSign",
        0x10020A3: "FFrancSign",
        0x10020A4: "LiraSign",
        0x10020A5: "MillSign",
        0x10020A6: "NairaSign",
        0x10020A7: "PesetaSign",
        0x10020A8: "RupeeSign",
        0x10020A9: "WonSign",
        0x10020AA: "NewSheqelSign",
        0x10020AB: "DongSign",
        0x20AC: "EuroSign",
        0x1002070: "zerosuperior",
        0x1002074: "foursuperior",
        0x1002075: "fivesuperior",
        0x1002076: "sixsuperior",
        0x1002077: "sevensuperior",
        0x1002078: "eightsuperior",
        0x1002079: "ninesuperior",
        0x1002080: "zerosubscript",
        0x1002081: "onesubscript",
        0x1002082: "twosubscript",
        0x1002083: "threesubscript",
        0x1002084: "foursubscript",
        0x1002085: "fivesubscript",
        0x1002086: "sixsubscript",
        0x1002087: "sevensubscript",
        0x1002088: "eightsubscript",
        0x1002089: "ninesubscript",
        0x1002202: "partdifferential",
        0x1002205: "emptyset",
        0x1002208: "elementof",
        0x1002209: "notelementof",
        0x100220B: "containsas",
        0x100221A: "squareroot",
        0x100221B: "cuberoot",
        0x100221C: "fourthroot",
        0x100222C: "dintegral",
        0x100222D: "tintegral",
        0x1002235: "because",
        0x1002248: "approxeq",
        0x1002247: "notapproxeq",
        0x1002262: "notidentical",
        0x1002263: "stricteq",
        0xFFF1: "braille_dot_1",
        0xFFF2: "braille_dot_2",
        0xFFF3: "braille_dot_3",
        0xFFF4: "braille_dot_4",
        0xFFF5: "braille_dot_5",
        0xFFF6: "braille_dot_6",
        0xFFF7: "braille_dot_7",
        0xFFF8: "braille_dot_8",
        0xFFF9: "braille_dot_9",
        0xFFFA: "braille_dot_10",
        0x1002800: "braille_blank",
        0x1002801: "braille_dots_1",
        0x1002802: "braille_dots_2",
        0x1002803: "braille_dots_12",
        0x1002804: "braille_dots_3",
        0x1002805: "braille_dots_13",
        0x1002806: "braille_dots_23",
        0x1002807: "braille_dots_123",
        0x1002808: "braille_dots_4",
        0x1002809: "braille_dots_14",
        0x100280A: "braille_dots_24",
        0x100280B: "braille_dots_124",
        0x100280C: "braille_dots_34",
        0x100280D: "braille_dots_134",
        0x100280E: "braille_dots_234",
        0x100280F: "braille_dots_1234",
        0x1002810: "braille_dots_5",
        0x1002811: "braille_dots_15",
        0x1002812: "braille_dots_25",
        0x1002813: "braille_dots_125",
        0x1002814: "braille_dots_35",
        0x1002815: "braille_dots_135",
        0x1002816: "braille_dots_235",
        0x1002817: "braille_dots_1235",
        0x1002818: "braille_dots_45",
        0x1002819: "braille_dots_145",
        0x100281A: "braille_dots_245",
        0x100281B: "braille_dots_1245",
        0x100281C: "braille_dots_345",
        0x100281D: "braille_dots_1345",
        0x100281E: "braille_dots_2345",
        0x100281F: "braille_dots_12345",
        0x1002820: "braille_dots_6",
        0x1002821: "braille_dots_16",
        0x1002822: "braille_dots_26",
        0x1002823: "braille_dots_126",
        0x1002824: "braille_dots_36",
        0x1002825: "braille_dots_136",
        0x1002826: "braille_dots_236",
        0x1002827: "braille_dots_1236",
        0x1002828: "braille_dots_46",
        0x1002829: "braille_dots_146",
        0x100282A: "braille_dots_246",
        0x100282B: "braille_dots_1246",
        0x100282C: "braille_dots_346",
        0x100282D: "braille_dots_1346",
        0x100282E: "braille_dots_2346",
        0x100282F: "braille_dots_12346",
        0x1002830: "braille_dots_56",
        0x1002831: "braille_dots_156",
        0x1002832: "braille_dots_256",
        0x1002833: "braille_dots_1256",
        0x1002834: "braille_dots_356",
        0x1002835: "braille_dots_1356",
        0x1002836: "braille_dots_2356",
        0x1002837: "braille_dots_12356",
        0x1002838: "braille_dots_456",
        0x1002839: "braille_dots_1456",
        0x100283A: "braille_dots_2456",
        0x100283B: "braille_dots_12456",
        0x100283C: "braille_dots_3456",
        0x100283D: "braille_dots_13456",
        0x100283E: "braille_dots_23456",
        0x100283F: "braille_dots_123456",
        0x1002840: "braille_dots_7",
        0x1002841: "braille_dots_17",
        0x1002842: "braille_dots_27",
        0x1002843: "braille_dots_127",
        0x1002844: "braille_dots_37",
        0x1002845: "braille_dots_137",
        0x1002846: "braille_dots_237",
        0x1002847: "braille_dots_1237",
        0x1002848: "braille_dots_47",
        0x1002849: "braille_dots_147",
        0x100284A: "braille_dots_247",
        0x100284B: "braille_dots_1247",
        0x100284C: "braille_dots_347",
        0x100284D: "braille_dots_1347",
        0x100284E: "braille_dots_2347",
        0x100284F: "braille_dots_12347",
        0x1002850: "braille_dots_57",
        0x1002851: "braille_dots_157",
        0x1002852: "braille_dots_257",
        0x1002853: "braille_dots_1257",
        0x1002854: "braille_dots_357",
        0x1002855: "braille_dots_1357",
        0x1002856: "braille_dots_2357",
        0x1002857: "braille_dots_12357",
        0x1002858: "braille_dots_457",
        0x1002859: "braille_dots_1457",
        0x100285A: "braille_dots_2457",
        0x100285B: "braille_dots_12457",
        0x100285C: "braille_dots_3457",
        0x100285D: "braille_dots_13457",
        0x100285E: "braille_dots_23457",
        0x100285F: "braille_dots_123457",
        0x1002860: "braille_dots_67",
        0x1002861: "braille_dots_167",
        0x1002862: "braille_dots_267",
        0x1002863: "braille_dots_1267",
        0x1002864: "braille_dots_367",
        0x1002865: "braille_dots_1367",
        0x1002866: "braille_dots_2367",
        0x1002867: "braille_dots_12367",
        0x1002868: "braille_dots_467",
        0x1002869: "braille_dots_1467",
        0x100286A: "braille_dots_2467",
        0x100286B: "braille_dots_12467",
        0x100286C: "braille_dots_3467",
        0x100286D: "braille_dots_13467",
        0x100286E: "braille_dots_23467",
        0x100286F: "braille_dots_123467",
        0x1002870: "braille_dots_567",
        0x1002871: "braille_dots_1567",
        0x1002872: "braille_dots_2567",
        0x1002873: "braille_dots_12567",
        0x1002874: "braille_dots_3567",
        0x1002875: "braille_dots_13567",
        0x1002876: "braille_dots_23567",
        0x1002877: "braille_dots_123567",
        0x1002878: "braille_dots_4567",
        0x1002879: "braille_dots_14567",
        0x100287A: "braille_dots_24567",
        0x100287B: "braille_dots_124567",
        0x100287C: "braille_dots_34567",
        0x100287D: "braille_dots_134567",
        0x100287E: "braille_dots_234567",
        0x100287F: "braille_dots_1234567",
        0x1002880: "braille_dots_8",
        0x1002881: "braille_dots_18",
        0x1002882: "braille_dots_28",
        0x1002883: "braille_dots_128",
        0x1002884: "braille_dots_38",
        0x1002885: "braille_dots_138",
        0x1002886: "braille_dots_238",
        0x1002887: "braille_dots_1238",
        0x1002888: "braille_dots_48",
        0x1002889: "braille_dots_148",
        0x100288A: "braille_dots_248",
        0x100288B: "braille_dots_1248",
        0x100288C: "braille_dots_348",
        0x100288D: "braille_dots_1348",
        0x100288E: "braille_dots_2348",
        0x100288F: "braille_dots_12348",
        0x1002890: "braille_dots_58",
        0x1002891: "braille_dots_158",
        0x1002892: "braille_dots_258",
        0x1002893: "braille_dots_1258",
        0x1002894: "braille_dots_358",
        0x1002895: "braille_dots_1358",
        0x1002896: "braille_dots_2358",
        0x1002897: "braille_dots_12358",
        0x1002898: "braille_dots_458",
        0x1002899: "braille_dots_1458",
        0x100289A: "braille_dots_2458",
        0x100289B: "braille_dots_12458",
        0x100289C: "braille_dots_3458",
        0x100289D: "braille_dots_13458",
        0x100289E: "braille_dots_23458",
        0x100289F: "braille_dots_123458",
        0x10028A0: "braille_dots_68",
        0x10028A1: "braille_dots_168",
        0x10028A2: "braille_dots_268",
        0x10028A3: "braille_dots_1268",
        0x10028A4: "braille_dots_368",
        0x10028A5: "braille_dots_1368",
        0x10028A6: "braille_dots_2368",
        0x10028A7: "braille_dots_12368",
        0x10028A8: "braille_dots_468",
        0x10028A9: "braille_dots_1468",
        0x10028AA: "braille_dots_2468",
        0x10028AB: "braille_dots_12468",
        0x10028AC: "braille_dots_3468",
        0x10028AD: "braille_dots_13468",
        0x10028AE: "braille_dots_23468",
        0x10028AF: "braille_dots_123468",
        0x10028B0: "braille_dots_568",
        0x10028B1: "braille_dots_1568",
        0x10028B2: "braille_dots_2568",
        0x10028B3: "braille_dots_12568",
        0x10028B4: "braille_dots_3568",
        0x10028B5: "braille_dots_13568",
        0x10028B6: "braille_dots_23568",
        0x10028B7: "braille_dots_123568",
        0x10028B8: "braille_dots_4568",
        0x10028B9: "braille_dots_14568",
        0x10028BA: "braille_dots_24568",
        0x10028BB: "braille_dots_124568",
        0x10028BC: "braille_dots_34568",
        0x10028BD: "braille_dots_134568",
        0x10028BE: "braille_dots_234568",
        0x10028BF: "braille_dots_1234568",
        0x10028C0: "braille_dots_78",
        0x10028C1: "braille_dots_178",
        0x10028C2: "braille_dots_278",
        0x10028C3: "braille_dots_1278",
        0x10028C4: "braille_dots_378",
        0x10028C5: "braille_dots_1378",
        0x10028C6: "braille_dots_2378",
        0x10028C7: "braille_dots_12378",
        0x10028C8: "braille_dots_478",
        0x10028C9: "braille_dots_1478",
        0x10028CA: "braille_dots_2478",
        0x10028CB: "braille_dots_12478",
        0x10028CC: "braille_dots_3478",
        0x10028CD: "braille_dots_13478",
        0x10028CE: "braille_dots_23478",
        0x10028CF: "braille_dots_123478",
        0x10028D0: "braille_dots_578",
        0x10028D1: "braille_dots_1578",
        0x10028D2: "braille_dots_2578",
        0x10028D3: "braille_dots_12578",
        0x10028D4: "braille_dots_3578",
        0x10028D5: "braille_dots_13578",
        0x10028D6: "braille_dots_23578",
        0x10028D7: "braille_dots_123578",
        0x10028D8: "braille_dots_4578",
        0x10028D9: "braille_dots_14578",
        0x10028DA: "braille_dots_24578",
        0x10028DB: "braille_dots_124578",
        0x10028DC: "braille_dots_34578",
        0x10028DD: "braille_dots_134578",
        0x10028DE: "braille_dots_234578",
        0x10028DF: "braille_dots_1234578",
        0x10028E0: "braille_dots_678",
        0x10028E1: "braille_dots_1678",
        0x10028E2: "braille_dots_2678",
        0x10028E3: "braille_dots_12678",
        0x10028E4: "braille_dots_3678",
        0x10028E5: "braille_dots_13678",
        0x10028E6: "braille_dots_23678",
        0x10028E7: "braille_dots_123678",
        0x10028E8: "braille_dots_4678",
        0x10028E9: "braille_dots_14678",
        0x10028EA: "braille_dots_24678",
        0x10028EB: "braille_dots_124678",
        0x10028EC: "braille_dots_34678",
        0x10028ED: "braille_dots_134678",
        0x10028EE: "braille_dots_234678",
        0x10028EF: "braille_dots_1234678",
        0x10028F0: "braille_dots_5678",
        0x10028F1: "braille_dots_15678",
        0x10028F2: "braille_dots_25678",
        0x10028F3: "braille_dots_125678",
        0x10028F4: "braille_dots_35678",
        0x10028F5: "braille_dots_135678",
        0x10028F6: "braille_dots_235678",
        0x10028F7: "braille_dots_1235678",
        0x10028F8: "braille_dots_45678",
        0x10028F9: "braille_dots_145678",
        0x10028FA: "braille_dots_245678",
        0x10028FB: "braille_dots_1245678",
        0x10028FC: "braille_dots_345678",
        0x10028FD: "braille_dots_1345678",
        0x10028FE: "braille_dots_2345678",
        0x10028FF: "braille_dots_12345678",
        0x1000D82: "Sinh_ng",
        0x1000D83: "Sinh_h2",
        0x1000D85: "Sinh_a",
        0x1000D86: "Sinh_aa",
        0x1000D87: "Sinh_ae",
        0x1000D88: "Sinh_aee",
        0x1000D89: "Sinh_i",
        0x1000D8A: "Sinh_ii",
        0x1000D8B: "Sinh_u",
        0x1000D8C: "Sinh_uu",
        0x1000D8D: "Sinh_ri",
        0x1000D8E: "Sinh_rii",
        0x1000D8F: "Sinh_lu",
        0x1000D90: "Sinh_luu",
        0x1000D91: "Sinh_e",
        0x1000D92: "Sinh_ee",
        0x1000D93: "Sinh_ai",
        0x1000D94: "Sinh_o",
        0x1000D95: "Sinh_oo",
        0x1000D96: "Sinh_au",
        0x1000D9A: "Sinh_ka",
        0x1000D9B: "Sinh_kha",
        0x1000D9C: "Sinh_ga",
        0x1000D9D: "Sinh_gha",
        0x1000D9E: "Sinh_ng2",
        0x1000D9F: "Sinh_nga",
        0x1000DA0: "Sinh_ca",
        0x1000DA1: "Sinh_cha",
        0x1000DA2: "Sinh_ja",
        0x1000DA3: "Sinh_jha",
        0x1000DA4: "Sinh_nya",
        0x1000DA5: "Sinh_jnya",
        0x1000DA6: "Sinh_nja",
        0x1000DA7: "Sinh_tta",
        0x1000DA8: "Sinh_ttha",
        0x1000DA9: "Sinh_dda",
        0x1000DAA: "Sinh_ddha",
        0x1000DAB: "Sinh_nna",
        0x1000DAC: "Sinh_ndda",
        0x1000DAD: "Sinh_tha",
        0x1000DAE: "Sinh_thha",
        0x1000DAF: "Sinh_dha",
        0x1000DB0: "Sinh_dhha",
        0x1000DB1: "Sinh_na",
        0x1000DB3: "Sinh_ndha",
        0x1000DB4: "Sinh_pa",
        0x1000DB5: "Sinh_pha",
        0x1000DB6: "Sinh_ba",
        0x1000DB7: "Sinh_bha",
        0x1000DB8: "Sinh_ma",
        0x1000DB9: "Sinh_mba",
        0x1000DBA: "Sinh_ya",
        0x1000DBB: "Sinh_ra",
        0x1000DBD: "Sinh_la",
        0x1000DC0: "Sinh_va",
        0x1000DC1: "Sinh_sha",
        0x1000DC2: "Sinh_ssha",
        0x1000DC3: "Sinh_sa",
        0x1000DC4: "Sinh_ha",
        0x1000DC5: "Sinh_lla",
        0x1000DC6: "Sinh_fa",
        0x1000DCA: "Sinh_al",
        0x1000DCF: "Sinh_aa2",
        0x1000DD0: "Sinh_ae2",
        0x1000DD1: "Sinh_aee2",
        0x1000DD2: "Sinh_i2",
        0x1000DD3: "Sinh_ii2",
        0x1000DD4: "Sinh_u2",
        0x1000DD6: "Sinh_uu2",
        0x1000DD8: "Sinh_ru2",
        0x1000DD9: "Sinh_e2",
        0x1000DDA: "Sinh_ee2",
        0x1000DDB: "Sinh_ai2",
        0x1000DDC: "Sinh_o2",
        0x1000DDD: "Sinh_oo2",
        0x1000DDE: "Sinh_au2",
        0x1000DDF: "Sinh_lu2",
        0x1000DF2: "Sinh_ruu2",
        0x1000DF3: "Sinh_luu2",
        0x1000DF4: "Sinh_kunddaliya",
    };

    /**
     * All keysyms which should not repeat when held down.
     * @private
     */
    var no_repeat = {
        0xFE03: true, // ISO Level 3 Shift (AltGr)
        0xFFE1: true, // Left shift
        0xFFE2: true, // Right shift
        0xFFE3: true, // Left ctrl
        0xFFE4: true, // Right ctrl
        0xFFE7: true, // Left meta
        0xFFE8: true, // Right meta
        0xFFE9: true, // Left alt
        0xFFEA: true, // Right alt
        0xFFEB: true, // Left hyper
        0xFFEC: true  // Right hyper
    };

    /**
     * All modifiers and their states.
     */
    this.modifiers = new Keyboard.ModifierState();

    /**
     * The state of every key, indexed by keysym. If a particular key is
     * pressed, the value of pressed for that keysym will be true. If a key
     * is not currently pressed, it will not be defined.
     */
    this.pressed = {};

    /**
     * The last result of calling the onkeydown handler for each key, indexed
     * by keysym. This is used to prevent/allow default actions for key events,
     * even when the onkeydown handler cannot be called again because the key
     * is (theoretically) still pressed.
     *
     * @private
     */
    var last_keydown_result = {};

    /**
     * The keysym most recently associated with a given keycode when keydown
     * fired. This object maps keycodes to keysyms.
     *
     * @private
     * @type {Object.<Number, Number>}
     */
    var recentKeysym = {};

    /**
     * Timeout before key repeat starts.
     * @private
     */
    var key_repeat_timeout = null;

    /**
     * Interval which presses and releases the last key pressed while that
     * key is still being held down.
     * @private
     */
    var key_repeat_interval = null;

    /**
     * Given an array of keysyms indexed by location, returns the keysym
     * for the given location, or the keysym for the standard location if
     * undefined.
     *
     * @private
     * @param {Number[]} keysyms
     *     An array of keysyms, where the index of the keysym in the array is
     *     the location value.
     *
     * @param {Number} location
     *     The location on the keyboard corresponding to the key pressed, as
     *     defined at: http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
     */
    var get_keysym = function get_keysym(keysyms, location) {

        if (!keysyms)
            return null;

        return keysyms[location] || keysyms[0];
    };

    /**
     * Returns true if the given keysym corresponds to a printable character,
     * false otherwise.
     *
     * @param {Number} keysym
     *     The keysym to check.
     *
     * @returns {Boolean}
     *     true if the given keysym corresponds to a printable character,
     *     false otherwise.
     */
    var isPrintable = function isPrintable(keysym) {

        // Keysyms with Unicode equivalents are printable
        return (keysym >= 0x00 && keysym <= 0xFF)
            || (keysym & 0xFFFF0000) === 0x01000000;

    };

    function keysym_from_key_identifier(identifier, location, shifted) {

        if (!identifier)
            return null;

        var typedCharacter;

        // If identifier is U+xxxx, decode Unicode character
        var unicodePrefixLocation = identifier.indexOf("U+");
        if (unicodePrefixLocation >= 0) {
            var hex = identifier.substring(unicodePrefixLocation+2);
            typedCharacter = String.fromCharCode(parseInt(hex, 16));
        }

        // If single character and not keypad, use that as typed character
        else if (identifier.length === 1 && location !== 3)
            typedCharacter = identifier;

        // Otherwise, look up corresponding keysym
        else
            return get_keysym(keyidentifier_keysym[identifier], location);

        // Alter case if necessary
        if (shifted === true)
            typedCharacter = typedCharacter.toUpperCase();
        else if (shifted === false)
            typedCharacter = typedCharacter.toLowerCase();

        // Get codepoint
        var codepoint = typedCharacter.charCodeAt(0);
        return keysym_from_charcode(codepoint);

    }

    function isControlCharacter(codepoint) {
        return codepoint <= 0x1F || (codepoint >= 0x7F && codepoint <= 0x9F);
    }

    function keysym_from_charcode(codepoint) {

        // Keysyms for control characters
        if (isControlCharacter(codepoint)) return 0xFF00 | codepoint;

        // Keysyms for ASCII chars
        if (codepoint >= 0x0000 && codepoint <= 0x00FF)
            return codepoint;

        // Keysyms for Unicode
        if (codepoint >= 0x0100 && codepoint <= 0x10FFFF)
            return 0x01000000 | codepoint;

        return null;

    }

    function keysym_from_keycode(keyCode, location) {
        return get_keysym(keycodeKeysyms[keyCode], location);
    }

    /**
     * Heuristically detects if the legacy keyIdentifier property of
     * a keydown/keyup event looks incorrectly derived. Chrome, and
     * presumably others, will produce the keyIdentifier by assuming
     * the keyCode is the Unicode codepoint for that key. This is not
     * correct in all cases.
     *
     * @private
     * @param {Number} keyCode
     *     The keyCode from a browser keydown/keyup event.
     *
     * @param {String} keyIdentifier
     *     The legacy keyIdentifier from a browser keydown/keyup event.
     *
     * @returns {Boolean}
     *     true if the keyIdentifier looks sane, false if the keyIdentifier
     *     appears incorrectly derived or is missing entirely.
     */
    var key_identifier_sane = function key_identifier_sane(keyCode, keyIdentifier) {

        // Missing identifier is not sane
        if (!keyIdentifier)
            return false;

        // Assume non-Unicode keyIdentifier values are sane
        var unicodePrefixLocation = keyIdentifier.indexOf("U+");
        if (unicodePrefixLocation === -1)
            return true;

        // If the Unicode codepoint isn't identical to the keyCode,
        // then the identifier is likely correct
        var codepoint = parseInt(keyIdentifier.substring(unicodePrefixLocation+2), 16);
        if (keyCode !== codepoint)
            return true;

        // The keyCodes for A-Z and 0-9 are actually identical to their
        // Unicode codepoints
        if ((keyCode >= 65 && keyCode <= 90) || (keyCode >= 48 && keyCode <= 57))
            return true;

        // The keyIdentifier does NOT appear sane
        return false;

    };

    /**
     * Marks a key as pressed, firing the keydown event if registered. Key
     * repeat for the pressed key will start after a delay if that key is
     * not a modifier. The return value of this function depends on the
     * return value of the keydown event handler, if any.
     *
     * @param {Number} keysym The keysym of the key to press.
     * @return {Boolean} true if event should NOT be canceled, false otherwise.
     */
    this.press = function(keysym) {

        // Don't bother with pressing the key if the key is unknown
        if (keysym === null) return;

        // Only press if released
        if (!guac_keyboard.pressed[keysym]) {

            // Mark key as pressed
            guac_keyboard.pressed[keysym] = true;

            // Send key event
            if (guac_keyboard.onkeydown) {

                var result = guac_keyboard.onkeydown(keysym_to_string[keysym],
                    modifier_state_to_str());
                last_keydown_result[keysym] = result;

                // Stop any current repeat
                window.clearTimeout(key_repeat_timeout);
                window.clearInterval(key_repeat_interval);

                // Repeat after a delay as long as pressed
                if (!no_repeat[keysym])
                    key_repeat_timeout = window.setTimeout(function() {
                        key_repeat_interval = window.setInterval(function() {
                            var mods = modifier_state_to_str();
                            guac_keyboard.onkeyup(keysym_to_string[keysym], mods);
                            guac_keyboard.onkeydown(keysym_to_string[keysym], mods);
                        }, 50);
                    }, 500);

                return result;
            }
        }

        // Return the last keydown result by default, resort to false if unknown
        return last_keydown_result[keysym] || false;

    };

    /**
     * Marks a key as released, firing the keyup event if registered.
     *
     * @param {Number} keysym The keysym of the key to release.
     */
    this.release = function(keysym) {

        // Only release if pressed
        if (guac_keyboard.pressed[keysym]) {

            // Mark key as released
            delete guac_keyboard.pressed[keysym];

            // Stop repeat
            window.clearTimeout(key_repeat_timeout);
            window.clearInterval(key_repeat_interval);

            // Send key event
            if (keysym !== null && guac_keyboard.onkeyup)
                guac_keyboard.onkeyup(keysym_to_string[keysym],
                    modifier_state_to_str());

        }

    };

    /**
     * Resets the state of this keyboard, releasing all keys, and firing keyup
     * events for each released key.
     */
    this.reset = function() {

        // Release all pressed keys
        for (var keysym in guac_keyboard.pressed)
            guac_keyboard.release(parseInt(keysym));

        // Clear event log
        eventLog = [];

    };

    /**
     * Given a keyboard event, updates the local modifier state and remote
     * key state based on the modifier flags within the event. This function
     * pays no attention to keycodes.
     *
     * @private
     * @param {KeyboardEvent} e
     *     The keyboard event containing the flags to update.
     */
    var update_modifier_state = function update_modifier_state(e) {

        // Get state
        var state = Keyboard.ModifierState.fromKeyboardEvent(e);

        // Release alt if implicitly released
        if (guac_keyboard.modifiers.alt && state.alt === false) {
            guac_keyboard.release(0xFFE9); // Left alt
            guac_keyboard.release(0xFFEA); // Right alt
            guac_keyboard.release(0xFE03); // AltGr
        }

        // Release shift if implicitly released
        if (guac_keyboard.modifiers.shift && state.shift === false) {
            guac_keyboard.release(0xFFE1); // Left shift
            guac_keyboard.release(0xFFE2); // Right shift
        }

        // Release ctrl if implicitly released
        if (guac_keyboard.modifiers.ctrl && state.ctrl === false) {
            guac_keyboard.release(0xFFE3); // Left ctrl
            guac_keyboard.release(0xFFE4); // Right ctrl
        }

        // Release meta if implicitly released
        if (guac_keyboard.modifiers.meta && state.meta === false) {
            guac_keyboard.release(0xFFE7); // Left meta
            guac_keyboard.release(0xFFE8); // Right meta
        }

        // Release hyper if implicitly released
        if (guac_keyboard.modifiers.hyper && state.hyper === false) {
            guac_keyboard.release(0xFFEB); // Left hyper
            guac_keyboard.release(0xFFEC); // Right hyper
        }

        // Update state
        guac_keyboard.modifiers = state;

    };

    /**
     * Constructs a string representing all currently pressed modifiers.
     *
     * @return {String} The resulting string.
     */
    var modifier_state_to_str = function modifier_state_to_str() {
        let masks = []
        if (guac_keyboard.modifiers.alt) masks.push("alt-mask");
        if (guac_keyboard.modifiers.ctrl) masks.push("control-mask");
        if (guac_keyboard.modifiers.meta) masks.push("meta-mask");
        if (guac_keyboard.modifiers.shift) masks.push("shift-mask");
        if (guac_keyboard.modifiers.hyper) masks.push("hyper-mask");
        return masks.join('+')
    }

    /**
     * Reads through the event log, removing events from the head of the log
     * when the corresponding true key presses are known (or as known as they
     * can be).
     *
     * @private
     * @return {Boolean} Whether the default action of the latest event should
     *                   be prevented.
     */
    function interpret_events() {

        // Do not prevent default if no event could be interpreted
        var handled_event = interpret_event();
        if (!handled_event)
            return false;

        // Interpret as much as possible
        var last_event;
        do {
            last_event = handled_event;
            handled_event = interpret_event();
        } while (handled_event !== null);

        return last_event.defaultPrevented;

    }

    /**
     * Releases Ctrl+Alt, if both are currently pressed and the given keysym
     * looks like a key that may require AltGr.
     *
     * @private
     * @param {Number} keysym The key that was just pressed.
     */
    var release_simulated_altgr = function release_simulated_altgr(keysym) {

        // Both Ctrl+Alt must be pressed if simulated AltGr is in use
        if (!guac_keyboard.modifiers.ctrl || !guac_keyboard.modifiers.alt)
            return;

        // Assume [A-Z] never require AltGr
        if (keysym >= 0x0041 && keysym <= 0x005A)
            return;

        // Assume [a-z] never require AltGr
        if (keysym >= 0x0061 && keysym <= 0x007A)
            return;

        // Release Ctrl+Alt if the keysym is printable
        if (keysym <= 0xFF || (keysym & 0xFF000000) === 0x01000000) {
            guac_keyboard.release(0xFFE3); // Left ctrl
            guac_keyboard.release(0xFFE4); // Right ctrl
            guac_keyboard.release(0xFFE9); // Left alt
            guac_keyboard.release(0xFFEA); // Right alt
        }

    };

    /**
     * Reads through the event log, interpreting the first event, if possible,
     * and returning that event. If no events can be interpreted, due to a
     * total lack of events or the need for more events, null is returned. Any
     * interpreted events are automatically removed from the log.
     *
     * @private
     * @return {KeyEvent}
     *     The first key event in the log, if it can be interpreted, or null
     *     otherwise.
     */
    var interpret_event = function interpret_event() {

        // Peek at first event in log
        var first = eventLog[0];
        if (!first)
            return null;

        // Keydown event
        if (first instanceof KeydownEvent) {

            var keysym = null;
            var accepted_events = [];

            // If event itself is reliable, no need to wait for other events
            if (first.reliable) {
                keysym = first.keysym;
                accepted_events = eventLog.splice(0, 1);
            }

            // If keydown is immediately followed by a keypress, use the indicated character
            else if (eventLog[1] instanceof KeypressEvent) {
                keysym = eventLog[1].keysym;
                accepted_events = eventLog.splice(0, 2);
            }

            // If keydown is immediately followed by anything else, then no
            // keypress can possibly occur to clarify this event, and we must
            // handle it now
            else if (eventLog[1]) {
                keysym = first.keysym;
                accepted_events = eventLog.splice(0, 1);
            }

            // Fire a key press if valid events were found
            if (accepted_events.length > 0) {

                if (keysym) {

                    // Fire event
                    release_simulated_altgr(keysym);
                    var defaultPrevented = !guac_keyboard.press(keysym);
                    recentKeysym[first.keyCode] = keysym;

                    // If a key is pressed while meta is held down, the keyup will
                    // never be sent in Chrome, so send it now. (bug #108404)
                    if (guac_keyboard.modifiers.meta && keysym !== 0xFFE7 && keysym !== 0xFFE8)
                        guac_keyboard.release(keysym);

                    // Record whether default was prevented
                    for (var i=0; i<accepted_events.length; i++)
                        accepted_events[i].defaultPrevented = defaultPrevented;

                }

                return first;

            }

        } // end if keydown

        // Keyup event
        else if (first instanceof KeyupEvent) {

            // Release specific key if known
            var keysym = first.keysym;
            if (keysym) {
                guac_keyboard.release(keysym);
                first.defaultPrevented = true;
            }

            // Otherwise, fall back to releasing all keys
            else {
                guac_keyboard.reset();
                return first;
            }

            return eventLog.shift();

        } // end if keyup

        // Ignore any other type of event (keypress by itself is invalid)
        else
            return eventLog.shift();

        // No event interpreted
        return null;

    };

    /**
     * Returns the keyboard location of the key associated with the given
     * keyboard event. The location differentiates key events which otherwise
     * have the same keycode, such as left shift vs. right shift.
     *
     * @private
     * @param {KeyboardEvent} e
     *     A JavaScript keyboard event, as received through the DOM via a
     *     "keydown", "keyup", or "keypress" handler.
     *
     * @returns {Number}
     *     The location of the key event on the keyboard, as defined at:
     *     http://www.w3.org/TR/DOM-Level-3-Events/#events-KeyboardEvent
     */
    var getEventLocation = function getEventLocation(e) {

        // Use standard location, if possible
        if ('location' in e)
            return e.location;

        // Failing that, attempt to use deprecated keyLocation
        if ('keyLocation' in e)
            return e.keyLocation;

        // If no location is available, assume left side
        return 0;

    };

    // When key pressed
    element.addEventListener("keydown", function(e) {

        // Only intercept if handler set
        if (!guac_keyboard.onkeydown) return;

        var keyCode;
        if (window.event) keyCode = window.event.keyCode;
        else if (e.which) keyCode = e.which;

        // Fix modifier states
        update_modifier_state(e);

        // Ignore (but do not prevent) the "composition" keycode sent by some
        // browsers when an IME is in use (see: http://lists.w3.org/Archives/Public/www-dom/2010JulSep/att-0182/keyCode-spec.html)
        if (keyCode === 229)
            return;

        // Log event
        var keydownEvent = new KeydownEvent(keyCode, e.keyIdentifier, e.key, getEventLocation(e));
        eventLog.push(keydownEvent);

        // Interpret as many events as possible, prevent default if indicated
        if (interpret_events())
            e.preventDefault();

    }, true);

    // When key pressed
    element.addEventListener("keypress", function(e) {

        // Only intercept if handler set
        if (!guac_keyboard.onkeydown && !guac_keyboard.onkeyup) return;

        var charCode;
        if (window.event) charCode = window.event.keyCode;
        else if (e.which) charCode = e.which;

        // Fix modifier states
        update_modifier_state(e);

        // Log event
        var keypressEvent = new KeypressEvent(charCode);
        eventLog.push(keypressEvent);

        // Interpret as many events as possible, prevent default if indicated
        if (interpret_events())
            e.preventDefault();

    }, true);

    // When key released
    element.addEventListener("keyup", function(e) {

        // Only intercept if handler set
        if (!guac_keyboard.onkeyup) return;

        e.preventDefault();

        var keyCode;
        if (window.event) keyCode = window.event.keyCode;
        else if (e.which) keyCode = e.which;

        // Fix modifier states
        update_modifier_state(e);

        // Log event, call for interpretation
        var keyupEvent = new KeyupEvent(keyCode, e.keyIdentifier, e.key, getEventLocation(e));
        eventLog.push(keyupEvent);
        interpret_events();

    }, true);

};

/**
 * The state of all supported keyboard modifiers.
 * @constructor
 */
Keyboard.ModifierState = function() {

    /**
     * Whether shift is currently pressed.
     * @type {Boolean}
     */
    this.shift = false;

    /**
     * Whether ctrl is currently pressed.
     * @type {Boolean}
     */
    this.ctrl = false;

    /**
     * Whether alt is currently pressed.
     * @type {Boolean}
     */
    this.alt = false;

    /**
     * Whether meta (apple key) is currently pressed.
     * @type {Boolean}
     */
    this.meta = false;

    /**
     * Whether hyper (windows key) is currently pressed.
     * @type {Boolean}
     */
    this.hyper = false;

};

/**
 * Returns the modifier state applicable to the keyboard event given.
 *
 * @param {KeyboardEvent} e The keyboard event to read.
 * @returns {Keyboard.ModifierState} The current state of keyboard
 *                                             modifiers.
 */
Keyboard.ModifierState.fromKeyboardEvent = function(e) {

    var state = new Keyboard.ModifierState();

    // Assign states from old flags
    state.shift = e.shiftKey;
    state.ctrl  = e.ctrlKey;
    state.alt   = e.altKey;
    state.meta  = e.metaKey;

    // Use DOM3 getModifierState() for others
    if (e.getModifierState) {
        state.hyper = e.getModifierState("OS")
                   || e.getModifierState("Super")
                   || e.getModifierState("Hyper")
                   || e.getModifierState("Win");
    }

    return state;

};
