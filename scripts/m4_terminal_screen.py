"""Bounded VT screen observer for OpenTUI acceptance PTYs (no terminal replies).

Consumes display operations rather than searching the ANSI byte stream. Styling,
links and terminal queries do not contribute text. Unicode ambiguous characters
occupy one cell; combining marks and common wide/emoji characters retain their
terminal cell geometry. This observer is not a general interactive terminal.
"""
from __future__ import annotations

import codecs
import unicodedata


class TerminalScreen:
    def __init__(self, width: int, height: int):
        if not 1 <= width <= 4096 or not 1 <= height <= 4096 or width * height > 1_000_000:
            raise ValueError("terminal dimensions must describe a bounded nonempty screen")
        self.width, self.height = width, height
        self._cells = self._blank_screen()
        self._x = self._y = 0
        self._saved = (0, 0)
        self._top, self._bottom = 0, height - 1
        self._wrap = True
        self._pending_wrap = False
        self._origin = False
        self._insert = False
        self._alternate = None
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self._state = "text"
        self._sequence = ""
        self._last = None
        self._join = False

    @property
    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self._cells)

    def feed(self, data: bytes) -> None:
        for char in self._decoder.decode(data):
            self._consume(char)

    def _blank_screen(self):
        return [[" "] * self.width for _ in range(self.height)]

    def _consume(self, char: str) -> None:
        if self._state in ("string", "string_escape"):
            if char == "\x07" or char == "\x9c" or (self._state == "string_escape" and char == "\\"):
                self._state = "text"
            else:
                self._state = "string_escape" if char == "\x1b" else "string"
            return
        if char in ("\x18", "\x1a"):
            self._state = "text"
            self._sequence = ""
            return
        if char == "\x1b":
            self._state = "escape"
            return
        if self._state == "escape":
            self._state = "text"
            if char == "[":
                self._state, self._sequence = "csi", ""
            elif char in "]P^_X":
                self._state = "string"
            elif char in "()*+-%#":
                self._state = "escape_intermediate"
            elif char == "7":
                self._saved = self._x, self._y
            elif char == "8":
                self._move(*self._saved)
            elif char == "D":
                self._linefeed()
            elif char == "E":
                self._x = 0
                self._linefeed()
            elif char == "M":
                if self._y == self._top:
                    self._scroll(-1)
                else:
                    self._move(self._x, self._y - 1)
            elif char == "c":
                self._cells = self._blank_screen()
                self._top, self._bottom = 0, self.height - 1
                self._origin, self._insert, self._wrap = False, False, True
                self._alternate = None
                self._move(0, 0)
            return
        if self._state == "escape_intermediate":
            if "0" <= char <= "~":
                self._state = "text"
            return
        if self._state in ("csi", "csi_discard"):
            if "@" <= char <= "~":
                if self._state == "csi":
                    self._csi(char, self._sequence)
                self._state, self._sequence = "text", ""
            elif self._state == "csi":
                self._sequence += char
                if len(self._sequence) > 256:
                    self._state, self._sequence = "csi_discard", ""
            return
        if char == "\x9b":
            self._state, self._sequence = "csi", ""
        elif char in ("\x90", "\x9d", "\x9e", "\x9f"):
            self._state = "string"
        elif char == "\r":
            self._move(0, self._y)
        elif char in "\n\v\f":
            self._linefeed()
        elif char == "\b":
            self._move(self._x - 1, self._y)
        elif char == "\t":
            self._move(min(self.width - 1, (self._x // 8 + 1) * 8), self._y)
        elif ord(char) >= 32 and not 0x7f <= ord(char) <= 0x9f:
            self._put(char)

    def _move(self, x: int, y: int) -> None:
        self._x = max(0, min(self.width - 1, x))
        self._y = max(0, min(self.height - 1, y))
        self._pending_wrap = False
        self._last = None
        self._join = False

    def _scroll(self, count: int) -> None:
        rows = self._cells[self._top:self._bottom + 1]
        amount = min(abs(count), len(rows))
        blanks = [[" "] * self.width for _ in range(amount)]
        self._cells[self._top:self._bottom + 1] = rows[amount:] + blanks if count > 0 else blanks + rows[:len(rows) - amount]

    def _linefeed(self) -> None:
        if self._y == self._bottom:
            self._scroll(1)
        else:
            self._y = min(self.height - 1, self._y + 1)
        self._pending_wrap = False
        self._last = None
        self._join = False

    def _clear_cell(self, row: list[str], x: int) -> None:
        if row[x] == "" and x > 0:
            row[x - 1] = " "
        elif x + 1 < self.width and row[x + 1] == "":
            row[x + 1] = " "
        row[x] = " "

    def _erase(self, y: int, start: int, end: int) -> None:
        for x in range(max(0, start), min(self.width, end)):
            self._clear_cell(self._cells[y], x)

    def _put(self, char: str) -> None:
        combining = unicodedata.combining(char) or unicodedata.category(char) in ("Mn", "Me") or char == "\u200d" or 0x1f3fb <= ord(char) <= 0x1f3ff
        if (combining or self._join) and self._last is not None:
            x, y = self._last
            # Grapheme storage is bounded independently of screen dimensions.
            if len(self._cells[y][x]) < 64:
                self._cells[y][x] += char
            self._join = char == "\u200d"
            return
        if combining:
            return
        width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
        if self._pending_wrap or (width == 2 and self._x == self.width - 1 and self._wrap):
            if self._wrap:
                self._x = 0
                self._linefeed()
            self._pending_wrap = False
        if width > self.width - self._x:
            return
        row = self._cells[self._y]
        if self._insert:
            row[self._x:self._x] = [" "] * width
            del row[self.width:]
        self._erase(self._y, self._x, self._x + width)
        row[self._x] = char
        if width == 2:
            row[self._x + 1] = ""
        self._last = self._x, self._y
        self._x += width
        if self._x >= self.width:
            self._x = self.width - 1
            self._pending_wrap = self._wrap

    def _csi(self, final: str, raw: str) -> None:
        private = raw.startswith("?")
        fields = raw.lstrip("?<=>").split(";")
        # Colon-separated SGR and intermediates are styling/query operations.
        if any(field and not field.isdecimal() for field in fields):
            return
        values = [min(int(field), 1_000_000) if field else 0 for field in fields]
        n = (values[0] if values else 0) or 1
        first = values[0] if values else 0
        if final in "Hf":
            row = first or 1
            column = (values[1] if len(values) > 1 else 0) or 1
            self._move(column - 1, min(self._bottom, self._top + row - 1) if self._origin else row - 1)
        elif final in "ABCDEFG`ade":
            if final == "A": self._move(self._x, max(self._top if self._origin else 0, self._y - n))
            elif final in "Be": self._move(self._x, min(self._bottom if self._origin else self.height - 1, self._y + n))
            elif final in "Ca": self._move(self._x + n, self._y)
            elif final == "D": self._move(self._x - n, self._y)
            elif final == "E": self._move(0, self._y + n)
            elif final == "F": self._move(0, self._y - n)
            elif final in "G`": self._move(n - 1, self._y)
            elif final == "d": self._move(self._x, n - 1 + (self._top if self._origin else 0))
        elif final == "J":
            if first in (2, 3):
                self._cells = self._blank_screen() if first == 2 else self._cells
            elif first == 0:
                self._erase(self._y, self._x, self.width)
                for y in range(self._y + 1, self.height): self._erase(y, 0, self.width)
            elif first == 1:
                for y in range(self._y): self._erase(y, 0, self.width)
                self._erase(self._y, 0, self._x + 1)
        elif final == "K":
            if first in (0, 1, 2): self._erase(self._y, 0 if first else self._x, self._x + 1 if first == 1 else self.width)
        elif final == "X":
            self._erase(self._y, self._x, self._x + n)
        elif final in "@P":
            row = self._cells[self._y]
            amount = min(n, self.width - self._x)
            if row[self._x] == "":
                self._clear_cell(row, self._x)
            if final == "@":
                row[self._x:self._x] = [" "] * amount
                del row[self.width:]
            else:
                del row[self._x:self._x + amount]
                row.extend([" "] * amount)
        elif final in "LM" and self._top <= self._y <= self._bottom:
            top = self._top
            self._top = self._y
            self._scroll(-n if final == "L" else n)
            self._top = top
        elif final in "ST": self._scroll(n if final == "S" else -n)
        elif final == "r" and not private:
            bottom = (values[1] if len(values) > 1 else 0) or self.height
            if 1 <= n < bottom <= self.height:
                self._top, self._bottom = n - 1, bottom - 1
                self._move(0, self._top if self._origin else 0)
        elif final == "s": self._saved = self._x, self._y
        elif final == "u": self._move(*self._saved)
        elif final in "hl":
            for mode in values:
                enabled = final == "h"
                if private and mode == 7: self._wrap = enabled; self._pending_wrap = False
                elif private and mode == 6: self._origin = enabled; self._move(0, self._top if enabled else 0)
                elif not private and mode == 4: self._insert = enabled
                elif private and mode in (47, 1047, 1049):
                    if enabled and self._alternate is None:
                        self._alternate = self._cells, self._x, self._y, self._top, self._bottom
                        self._cells = self._blank_screen()
                        self._top, self._bottom = 0, self.height - 1
                        self._move(0, 0)
                    elif not enabled and self._alternate is not None:
                        self._cells, x, y, self._top, self._bottom = self._alternate
                        self._alternate = None
                        self._move(x, y)
