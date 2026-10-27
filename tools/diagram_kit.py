#!/usr/bin/env python3
"""Diagram kit: hand-laid SVG pages with measured text, printed to PDF by headless Chromium.

A generator script registers its improvement points, defines one function per page and hands
them to `main()` (see templates/example_diagrams.py):

    import diagram_kit as kit

    kit.register(kit.Improvement("N-01", "high", "api", "finding", "proposal", "short note"))

    def overview(p):                                  # p is a kit.Page
        core = p.box(100, 140, 300, 90, "core", "core", ["what it does"])
        app = p.box(100, 300, 300, 90, "app", "app")
        p.arrow((app.cx, app.y), (core.cx, core.b))   # from user to provider
        p.legend(kit.MARGIN, p.legend_y, ["core", "app"])

    kit.main([kit.Spec("01-overview", "Overview", "What depends on what.", overview)],
             out_dir="docs/architecture", project="my-project",
             generator="tools/architecture_diagrams.py")

Every label drawn inside a container is measured: text that would not fit raises LayoutError
and the build stops instead of printing an overflow. Widths assume a monospace font advancing
0.6 em per glyph (JetBrains Mono; Cascadia Mono and Consolas are narrower, so they still fit).

The PDF's dates are pinned ($SOURCE_DATE_EPOCH, else the last commit's time, else the generator
file's modification time), so rebuilding the same source with the same browser gives identical
bytes. Standard library only; printing needs Edge or Chrome (set CHROME to its path when it is
not found).
"""

import argparse
import html
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
from datetime import datetime, timezone
from pathlib import Path

MM = 96 / 25.4  # CSS pixels per millimetre; SVG user units are CSS pixels
PAGE_SIZES = {"A3": (420, 297), "A4": (297, 210)}  # landscape, millimetres
MARGIN = 40.0

FONT = ("'JetBrains Mono', 'JetBrainsMono Nerd Font', 'JetBrainsMono NF', 'Cascadia Mono', "
        "Consolas, monospace")
ADVANCE = 0.6  # em per glyph; the fit checks rely on it

BG = "#020617"
GRID = "#1e293b"
UNDERLAY = "#0f172a"
WHITE = "#f8fafc"
TEXT = "#cbd5e1"
SUB = "#94a3b8"
DIM = "#64748b"
FAINT = "#475569"
EDGE = "#64748b"
ROSE = "#fb7185"

# (fill, stroke) per kind of component. Generators may rename kinds in KIND_NAMES or add kinds.
KINDS = {
    "core": ("rgba(6, 78, 59, 0.4)", "#34d399"),         # library, engine or service module
    "app": ("rgba(8, 51, 68, 0.4)", "#22d3ee"),          # application, game, UI
    "data": ("rgba(76, 29, 149, 0.4)", "#a78bfa"),       # data, state, files, schemas
    "platform": ("rgba(120, 53, 15, 0.3)", "#fbbf24"),   # OS, GPU, cloud, network facing
    "external": ("rgba(30, 41, 59, 0.5)", "#94a3b8"),    # third-party, CI, repository
    "tool": ("rgba(124, 45, 18, 0.3)", "#fb923c"),       # offline tools and scripts
}
KIND_NAMES = {
    "core": "library module",
    "app": "application code",
    "data": "data: state, files, schemas",
    "platform": "OS, GPU or network facing",
    "external": "external dependency, CI, repository",
    "tool": "offline tool or script",
}
PRIORITIES = {"high": ROSE, "medium": "#fbbf24", "low": "#94a3b8"}


class LayoutError(Exception):
    """Text that does not fit where a page puts it."""


# ------------------------------------------------------------------- improvement points

class Improvement:
    """One improvement point: a badge on the pages, a row in the register."""

    def __init__(self, ident, priority, where, finding, proposal, short):
        if priority not in PRIORITIES:
            raise ValueError(f"{ident}: priority must be one of {list(PRIORITIES)}")
        self.ident, self.priority, self.where = ident, priority, where
        self.finding, self.proposal, self.short = finding, proposal, short


REGISTRY: dict = {}


def register(*items):
    """Make improvement points known to `Page.badge` and `improvement_register`."""
    for item in items:
        REGISTRY[item.ident] = item


# ------------------------------------------------------------------------------ helpers

def text_width(s, size):
    """Width of `s` in pixels at font size `size` (monospace)."""
    return len(s) * size * ADVANCE


def wrap(s, size, width):
    """`s` broken into lines that fit `width` pixels at `size`."""
    columns = max(1, int(width // (size * ADVANCE)))
    return textwrap.wrap(s, columns, break_long_words=False, break_on_hyphens=False) or [""]


def esc(s):
    return html.escape(s, quote=False)


def fmt(v):
    return f"{v:.1f}".rstrip("0").rstrip(".")


class Box:
    """Where something was drawn, for attaching arrows to its edges."""

    def __init__(self, x, y, w, h):
        self.x, self.y, self.w, self.h = x, y, w, h

    @property
    def r(self):
        return self.x + self.w

    @property
    def b(self):
        return self.y + self.h

    @property
    def cx(self):
        return self.x + self.w / 2

    @property
    def cy(self):
        return self.y + self.h / 2


# --------------------------------------------------------------------------------- page

class Page:
    """One landscape page: SVG elements in drawing order, plus the checks that keep text in.

    Layers, back to front: background, group boundaries, arrows, boxes, text, badges and chips.
    Arrows are drawn before boxes, and boxes have an opaque underlay, so arrow ends tuck under
    the boxes they connect. Content goes between `top` and `bottom`; `legend_y` is the legend's
    baseline; the frame (title, page number, footer) is added by `build`.
    """

    LAYERS = ("back", "groups", "edges", "nodes", "text", "top")

    def __init__(self, number, total, name, title, subtitle, size="A3"):
        self.number, self.total, self.name = number, total, name
        self.title, self.subtitle = title, subtitle
        self.size_mm = PAGE_SIZES[size]
        self.W, self.H = self.size_mm[0] * MM, self.size_mm[1] * MM
        self.top, self.bottom = 104.0, self.H - 67.5
        self.legend_y = self.H - 38.5
        self.layers = {layer: [] for layer in self.LAYERS}
        self.markers = {}

    # -------------------------------------------------------------- primitives

    def add(self, layer, element):
        self.layers[layer].append(element)

    def rect(self, x, y, w, h, fill, stroke=None, width=1.0, rx=6.0, dash=None, layer="nodes"):
        attrs = f' stroke="{stroke}" stroke-width="{fmt(width)}"' if stroke else ""
        if dash:
            attrs += f' stroke-dasharray="{dash}"'
        self.add(layer, f'<rect x="{fmt(x)}" y="{fmt(y)}" width="{fmt(w)}" height="{fmt(h)}" '
                        f'rx="{fmt(rx)}" fill="{fill}"{attrs}/>')

    def line(self, x1, y1, x2, y2, color, width=1.0, opacity=1.0, layer="nodes"):
        self.add(layer, f'<line x1="{fmt(x1)}" y1="{fmt(y1)}" x2="{fmt(x2)}" y2="{fmt(y2)}" '
                        f'stroke="{color}" stroke-width="{fmt(width)}" '
                        f'stroke-opacity="{fmt(opacity)}"/>')

    def circle(self, x, y, r, fill, layer="nodes"):
        self.add(layer, f'<circle cx="{fmt(x)}" cy="{fmt(y)}" r="{fmt(r)}" fill="{fill}"/>')

    def text(self, x, y, content, size=10.0, color=TEXT, weight=None, anchor="start", fit=None,
             layer="text"):
        """Text at baseline `y`. `content` is a string or a list of (text, colour, weight, size)
        spans. With `fit`, text wider than `fit` pixels raises LayoutError. Returns the width."""
        if isinstance(content, str):
            spans = [(content, color, weight, size)]
        else:
            spans = [(s[0], s[1] if len(s) > 1 else color, s[2] if len(s) > 2 else weight,
                      s[3] if len(s) > 3 else size) for s in content]
        width = sum(text_width(s, sz) for s, _, _, sz in spans)
        if fit is not None and width > fit + 0.01:
            joined = "".join(s for s, _, _, _ in spans)
            raise LayoutError(f"{self.name}: {joined!r} needs {width:.0f}px but has {fit:.0f}px")
        parts = []
        for s, c, w, sz in spans:
            bold = f' font-weight="{w}"' if w else ""
            parts.append(f'<tspan fill="{c}" font-size="{fmt(sz)}"{bold}>{esc(s)}</tspan>')
        anchor_attr = f' text-anchor="{anchor}"' if anchor != "start" else ""
        self.add(layer, f'<text x="{fmt(x)}" y="{fmt(y)}"{anchor_attr} '
                        f'xml:space="preserve">{"".join(parts)}</text>')
        return width

    def marker(self, color):
        ident = "arrow-" + color.strip("#")
        if ident not in self.markers:
            self.markers[ident] = (
                f'<marker id="{ident}" markerWidth="10" markerHeight="8" refX="9" refY="4" '
                f'orient="auto" markerUnits="userSpaceOnUse">'
                f'<polygon points="0 0, 10 4, 0 8" fill="{color}"/></marker>')
        return ident

    def arrow(self, *points, color=EDGE, width=1.4, dash=None, head=True, label=None,
              label_at=None, label_color=SUB, label_size=8.5, label_anchor="middle", gap=2.5):
        """A polyline through `points` (route it orthogonally by hand). The head stops `gap`
        short of the last point, so it touches the border it points at. A label gets a
        background patch so the line never runs through its text."""
        pts = list(points)
        if head and gap and len(pts) >= 2:
            (x1, y1), (x2, y2) = pts[-2], pts[-1]
            length = math.hypot(x2 - x1, y2 - y1)
            if length > gap:
                pts[-1] = (x2 - (x2 - x1) / length * gap, y2 - (y2 - y1) / length * gap)
        d = "M " + " L ".join(f"{fmt(x)} {fmt(y)}" for x, y in pts)
        attrs = f' stroke-dasharray="{dash}"' if dash else ""
        if head:
            attrs += f' marker-end="url(#{self.marker(color)})"'
        self.add("edges", f'<path d="{d}" fill="none" stroke="{color}" '
                          f'stroke-width="{fmt(width)}" stroke-linejoin="round"{attrs}/>')
        if label:
            x, y = label_at or pts[0]
            w = text_width(label, label_size)
            left = {"start": x, "middle": x - w / 2, "end": x - w}[label_anchor]
            self.rect(left - 3, y - label_size, w + 6, label_size + 4, BG, rx=3, layer="text")
            self.text(x, y, label, label_size, label_color, anchor=label_anchor)

    # -------------------------------------------------------------- components

    def box(self, x, y, w, h, kind, title=None, lines=(), *, header=None, size=9.0,
            title_size=11.0, align="center", pad=10.0, rx=6.0, dash=None, line_color=SUB):
        """A component: opaque underlay, the tinted fill and stroke of `kind`, then a vertically
        centred block of an optional small `header`, a bold `title` and `lines` (a line may be a
        (text, colour) pair). Raises LayoutError when the block is wider or taller than the box."""
        fill, stroke = KINDS[kind]
        self.rect(x, y, w, h, UNDERLAY, rx=rx)
        self.rect(x, y, w, h, fill, stroke=stroke, width=1.5, rx=rx, dash=dash)
        rows = []
        if header:
            rows.append((header, 8.5, stroke, 600, 8.5 * 1.6))
        if title:
            rows.append((title, title_size, WHITE, 700, title_size * 1.5))
        for line in lines:
            text, color = (line, line_color) if isinstance(line, str) else line
            rows.append((text, size, color, None, size * 1.5))
        total = sum(row[4] for row in rows)
        if total > h - 6:
            raise LayoutError(f"{self.name}: box {title or header!r} needs {total:.0f}px of "
                              f"height but has {h:.0f}px")
        top = y + (h - total) / 2
        for text, sz, color, weight, height in rows:
            baseline = top + height / 2 + sz * 0.36
            if align == "center":
                self.text(x + w / 2, baseline, text, sz, color, weight, "middle", fit=w - 2 * pad)
            else:
                self.text(x + pad, baseline, text, sz, color, weight, fit=w - 2 * pad)
            top += height
        return Box(x, y, w, h)

    def group(self, x, y, w, h, label, color=FAINT, label_color=SUB, dash="6,4"):
        """A dashed boundary with its label in the top-left corner. Keep badges off that corner:
        put them at the right edge (`badge(..., anchor="end")`)."""
        self.rect(x, y, w, h, "none", stroke=color, width=1, rx=10, dash=dash, layer="groups")
        self.text(x + 12, y + 17, label, 9.5, label_color, 600, fit=w - 24, layer="groups")
        return Box(x, y, w, h)

    def chip(self, x, y, label, kind="external", size=8.5):
        """A small tag (an external dependency); returns its width."""
        fill, stroke = KINDS[kind]
        w = text_width(label, size) + 12
        self.rect(x, y, w, 17, UNDERLAY, rx=4, layer="top")
        self.rect(x, y, w, 17, fill, stroke=stroke, width=1, rx=4, layer="top")
        self.text(x + 6, y + 12, label, size, TEXT, layer="top")
        return w

    def chip_row(self, x, y, chips, max_x):
        """Chips left to right; a chip is a label or (label, kind)."""
        for chip in chips:
            label, kind = (chip, "external") if isinstance(chip, str) else chip
            if x + text_width(label, 8.5) + 12 > max_x:
                raise LayoutError(f"{self.name}: chip {label!r} does not fit in its row")
            x += self.chip(x, y, label, kind) + 6

    def badge(self, x, y, ident, note=None, anchor="start", note_size=8.5, fit=None):
        """An improvement point at baseline `y`: a pill with its ID in the priority colour, then
        an optional short note. Returns the width used."""
        if ident not in REGISTRY:
            raise LayoutError(f"{self.name}: badge {ident!r} is not a registered improvement")
        color = PRIORITIES[REGISTRY[ident].priority]
        pill = text_width(ident, 9) + 12
        total = pill + (5 + text_width(note, note_size) if note else 0)
        if fit is not None and total > fit + 0.01:
            raise LayoutError(f"{self.name}: badge {ident} {note!r} needs {total:.0f}px, "
                              f"has {fit:.0f}px")
        if anchor == "end":
            x -= total
        self.rect(x, y - 11.5, pill, 15.5, color, rx=7.75, layer="top")
        self.text(x + pill / 2, y, ident, 9, "#0b1120", 700, "middle", layer="top")
        if note:
            self.text(x + pill + 5, y, note, note_size, color, layer="top")
        return total

    def badge_row(self, x, y, badges, max_x, gap=14.0):
        """(ID, note) badges left to right, all inside `max_x`."""
        for ident, note in badges:
            x += self.badge(x, y, ident, note, fit=max_x - x) + gap

    @staticmethod
    def card_height(w, items, size=9.0):
        """The height a card `w` pixels wide needs for `items`."""
        line_h = size * 1.55
        height = 42.0
        for item in items:
            if isinstance(item, tuple):
                height += line_h + 4
            else:
                height += line_h * len(wrap(item, size, w - 44))
        return height - line_h + 14

    def card(self, x, y, w, h, title, items, dot="#22d3ee", size=9.0):
        """A note card: title with a coloured dot, then wrapped bullet items. An item may be
        ("badge", ID, note). `h` None sizes the card to its content."""
        if h is None:
            h = self.card_height(w, items, size)
        self.rect(x, y, w, h, "rgba(15, 23, 42, 0.8)", stroke="#1e293b", width=1, rx=10)
        self.circle(x + 16, y + 18, 4, dot)
        self.text(x + 28, y + 22, title, 10.5, WHITE, 700, fit=w - 40)
        line_h = size * 1.55
        yy = y + 42
        for item in items:
            if isinstance(item, tuple):
                _, ident, note = item
                self.badge(x + 16, yy + 1, ident, note, fit=w - 32)
                yy += line_h + 4
                continue
            for i, line in enumerate(wrap(item, size, w - 44)):
                self.text(x + 16, yy, ("• " if i == 0 else "  ") + line, size, SUB, fit=w - 30)
                yy += line_h
        if yy - line_h + 8 > y + h:
            raise LayoutError(f"{self.name}: card {title!r} needs {yy - line_h + 8 - y:.0f}px "
                              f"of height but has {h:.0f}px")
        return Box(x, y, w, h)

    def cards(self, x, y, w, h, cards, gap=14.0, size=9.0, bottom=None):
        """(title, dot colour, items) cards side by side across `w`. With `h` None they are as
        tall as the fullest needs; with `bottom` they end there and `y` is ignored. Returns the
        row's Box, so the content above can fill what is left."""
        cw = (w - gap * (len(cards) - 1)) / len(cards)
        if h is None:
            h = max(self.card_height(cw, items, size) for _, _, items in cards)
        if bottom is not None:
            y = bottom - h
        for i, (title, dot, items) in enumerate(cards):
            self.card(x + i * (cw + gap), y, cw, h, title, items, dot, size)
        return Box(x, y, w, h)

    def legend(self, x, y, kinds, badges=True, note="(IDs: see the improvement register)"):
        """Colour keys for the kinds used on the page, then the improvement priorities."""
        for kind in kinds:
            fill, stroke = KINDS[kind]
            self.rect(x, y - 9, 16, 10, fill, stroke=stroke, width=1, rx=2, layer="text")
            x += 22 + self.text(x + 22, y, KIND_NAMES[kind], 8.5, SUB) + 18
        if badges:
            x += 6
            x += self.text(x, y, "improvement points:", 8.5, SUB) + 8
            for priority, color in PRIORITIES.items():
                self.circle(x + 4, y - 3, 4, color, layer="text")
                x += 12 + self.text(x + 12, y, priority, 8.5, SUB) + 12
            if note:
                x += self.text(x, y, note, 8.5, DIM)
        if x > self.W - MARGIN:
            raise LayoutError(f"{self.name}: the legend runs off the page")

    # -------------------------------------------------------------- composites

    def package(self, x, y, w, h, kind, name, tag, role, modules, chips, stats, badges=()):
        """A package (crate, module, service): name and tag, `stats` on the right, a role line,
        module lines, external dependencies as chips, and (ID, note) badges along the bottom."""
        box = self.box(x, y, w, h, kind)
        inner = w - 28
        self.text(x + 14, y + 26, [(name, WHITE, 700, 15), ("  " + tag, SUB, None, 9.5)],
                  fit=inner - text_width(stats, 9) - 12)
        self.text(x + w - 14, y + 26, stats, 9, DIM, anchor="end")
        self.text(x + 14, y + 46, role, 10, TEXT, fit=inner)
        yy = y + 63
        for line in modules:
            self.text(x + 14, yy, line, 9, SUB, fit=inner)
            yy += 14
        if chips:
            self.chip_row(x + 14, yy - 4, chips, x + w - 14)
            yy += 20
        if badges:
            if yy > y + h - 24:
                raise LayoutError(f"{self.name}: package {name!r} has no room for its badges")
            self.badge_row(x + 14, y + h - 12, badges, x + w - 14)
        elif yy > y + h:
            raise LayoutError(f"{self.name}: package {name!r} is too short for its content")
        return box

    PANEL_HEADER, SECTION_TITLE, ITEM_LINE, ITEM_SIZE, SECTION_GAP = 68.0, 16.0, 13.0, 9.5, 8.0

    @classmethod
    def panel_height(cls, sections):
        """Height `panel` needs for `sections`."""
        height = cls.PANEL_HEADER + 8
        for _, _, items, _ in sections:
            height += cls.SECTION_TITLE + cls.ITEM_LINE * len(items) + cls.SECTION_GAP
        return height

    def panel(self, x, y, w, h, kind, title, tagline, sections):
        """A package's modules and their public API. `sections` are (name, note, items, badge
        IDs); a note starting with "private" greys the module out."""
        _, stroke = KINDS[kind]
        self.box(x, y, w, h, kind)
        self.text(x + 14, y + 26, title, 15, WHITE, 700, fit=w - 28)
        self.text(x + 14, y + 44, tagline, 9.5, SUB, fit=w - 28)
        self.line(x + 14, y + 54, x + w - 14, y + 54, stroke, opacity=0.35)
        yy = y + self.PANEL_HEADER + 8
        for name, note, items, badges in sections:
            private = note.startswith("private")
            badge_w = sum(text_width(b, 9) + 12 + 6 for b in badges)
            spans: list = [(name, SUB if private else stroke, 700, 10.5)]
            if note:
                spans.append(("  " + note, DIM if private else SUB, None, 9))
            self.text(x + 14, yy, spans, fit=w - 28 - badge_w)
            bx = x + w - 14
            for ident in reversed(badges):
                bx -= self.badge(bx, yy, ident, anchor="end") + 6
            yy += self.SECTION_TITLE - 2
            for item in items:
                self.text(x + 26, yy, item, self.ITEM_SIZE, TEXT, fit=w - 40)
                yy += self.ITEM_LINE
            yy += self.SECTION_GAP + 2
        if yy - self.SECTION_GAP > y + h:
            raise LayoutError(f"{self.name}: panel {title!r} overflows its height")

    def matrix(self, x, y, bottom, rows, cols, cells, *, kinds=None, layer=None,
               forbidden=None, label_w=150.0, size=10.0, line_h=14.0, pad=12.0):
        """Who uses what: one row per user, one column per provider, each cell listing the API
        the row uses from the column. Rows stretch to fill down to `bottom`.

        `cells[(row, col)]` is a list of items; `kinds` maps a name to its kind (default "core");
        `layer` maps a name to its layer number, so empty cells that would point upwards say so;
        `forbidden[(row, col)]` is a list of lines explaining a dependency that must never exist.
        """
        kinds, layer, forbidden = kinds or {}, layer or {}, forbidden or {}
        col_w = (self.W - MARGIN - x - label_w) / len(cols)
        text_w = col_w - 2 * pad - 12
        header_h = 50.0

        def lines_of(row, col):
            out = []
            for item in cells.get((row, col), []):
                out += wrap(item, size, text_w)
            return out

        heights = [max(56.0, max(len(lines_of(r, c)) for c in cols) * line_h + 2 * pad + 4)
                   for r in rows]
        spare = bottom - (y + header_h + sum(heights))
        if spare < 0:
            raise LayoutError(f"{self.name}: the matrix is {-spare:.0f}px too tall")
        heights = [h + spare / len(heights) for h in heights]

        self.text(x + 8, y + 22, "user ↓", 9.5, DIM)
        self.text(x + 8, y + 39, "provider →", 9.5, DIM)
        for j, col in enumerate(cols):
            cx = x + label_w + j * col_w
            fill, stroke = KINDS[kinds.get(col, "core")]
            self.rect(cx + 2, y, col_w - 4, header_h - 4, fill, stroke=stroke, width=1, rx=6)
            self.text(cx + col_w / 2, y + 21, col, 12, WHITE, 700, "middle", fit=col_w - 12)
            self.text(cx + col_w / 2, y + 37, "provides", 9, SUB, anchor="middle")
        yy = y + header_h
        for row, h in zip(rows, heights):
            fill, stroke = KINDS[kinds.get(row, "core")]
            self.rect(x, yy + 2, label_w - 4, h - 4, fill, stroke=stroke, width=1, rx=6)
            self.text(x + 12, yy + h / 2 - 2, row, 12, WHITE, 700, fit=label_w - 24)
            self.text(x + 12, yy + h / 2 + 14, "uses →", 9, SUB)
            for j, col in enumerate(cols):
                cx = x + label_w + j * col_w
                lines = lines_of(row, col)
                if row == col:
                    self.rect(cx + 2, yy + 2, col_w - 4, h - 4, UNDERLAY, rx=6)
                    self.text(cx + col_w / 2, yy + h / 2 + 4, "—", 10, FAINT, anchor="middle")
                elif lines:
                    self.rect(cx + 2, yy + 2, col_w - 4, h - 4, "rgba(6, 78, 59, 0.18)",
                              stroke="#34d399", width=0.8, rx=6)
                    ty = yy + (h - len(lines) * line_h) / 2 + size * 0.95
                    for item in cells[(row, col)]:
                        for k, line in enumerate(wrap(item, size, text_w)):
                            self.text(cx + pad + (0 if k == 0 else 12), ty,
                                      ("· " if k == 0 else "") + line, size, TEXT,
                                      fit=col_w - 2 * pad)
                            ty += line_h
                elif (row, col) in forbidden:
                    self.rect(cx + 2, yy + 2, col_w - 4, h - 4, "rgba(136, 19, 55, 0.3)",
                              stroke=ROSE, width=1, rx=6, dash="4,3")
                    note = forbidden[(row, col)]
                    ty = yy + h / 2 - (len(note) - 1) * 7 + 4
                    for i, line in enumerate(note):
                        self.text(cx + col_w / 2, ty, line, 10.5 if i == 0 else 9.5,
                                  ROSE if i == 0 else SUB, 700 if i == 0 else None, "middle",
                                  fit=col_w - 16)
                        ty += 14
                else:
                    upwards = layer.get(col, 0) > layer.get(row, 0)
                    self.rect(cx + 2, yy + 2, col_w - 4, h - 4, "rgba(15, 23, 42, 0.55)",
                              stroke="#1e293b", width=0.8, rx=6)
                    self.text(cx + col_w / 2, yy + h / 2 + 4,
                              "× would point upwards" if upwards else "· allowed, not used",
                              9, FAINT if upwards else DIM, anchor="middle")
            yy += h
        return Box(x, y, self.W - MARGIN - x, yy - y)

    # -------------------------------------------------------------- frame and output

    def frame(self, project, commit, date, generator, footer_right=""):
        self.add("back", f'<rect width="{fmt(self.W)}" height="{fmt(self.H)}" fill="{BG}"/>')
        self.add("back", f'<rect width="{fmt(self.W)}" height="{fmt(self.H)}" '
                         f'fill="url(#grid)"/>')
        self.circle(MARGIN + 6, 45, 6, "#22d3ee", layer="text")
        room = self.W - 2 * MARGIN - 240
        self.text(MARGIN + 22, 53, self.title, 24, WHITE, 700, fit=room)
        self.text(MARGIN + 22, 77, self.subtitle, 11.5, SUB, fit=room)
        self.text(self.W - MARGIN, 53, f"{self.number:02d} / {self.total:02d}", 14, DIM, 600,
                  "end")
        self.text(self.W - MARGIN, 76, f"{project} architecture", 10, FAINT, anchor="end")
        self.text(MARGIN, self.H - 16, f"{project} · as of commit {commit} ({date}) · drawn by "
                  f"{generator}", 9, FAINT)
        if footer_right:
            self.text(self.W - MARGIN, self.H - 16, footer_right, 9, FAINT, anchor="end")

    def svg(self):
        defs = ['<pattern id="grid" width="40" height="40" patternUnits="userSpaceOnUse">'
                f'<path d="M 40 0 L 0 0 0 40" fill="none" stroke="{GRID}" stroke-width="0.5"/>'
                '</pattern>', *self.markers.values()]
        body = "\n".join(element for layer in self.LAYERS for element in self.layers[layer])
        return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {fmt(self.W)} '
                f'{fmt(self.H)}" font-family="{FONT}">\n<defs>{"".join(defs)}</defs>\n'
                f"{body}\n</svg>")

    def html(self, project):
        w_mm, h_mm = self.size_mm
        return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{esc(project)} architecture {self.number:02d}: {esc(self.title)}</title>
<style>
  @page {{ size: {w_mm}mm {h_mm}mm; margin: 0; }}
  html, body {{ margin: 0; padding: 0; background: {BG}; height: {h_mm}mm; overflow: hidden;
               -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
  svg {{ display: block; width: {w_mm}mm; height: {h_mm}mm; }}
</style>
</head>
<body>
{self.svg()}
</body>
</html>
"""


# ------------------------------------------------------------------- the register page

def improvement_register(p, areas, order, order_notes, x0=590.0, area_kinds=None):
    """The standard last page. Left: one box per area with its badges and counts per priority,
    and a "Suggested order" card ending in the order as a badge chain. Right: every registered
    improvement as a table row (ID, priority, where, finding, proposal).

    `areas` are (kind, name, [IDs]); `order` is a list of IDs; `order_notes` are the card's
    bullet lines."""
    left_w = x0 - MARGIN - 30
    p.text(MARGIN, p.top + 14, "where the improvement points are", 10, SUB, 600)
    y = p.top + 28
    card_h = 137.0
    for kind, name, idents in areas:
        if y + 86 > p.bottom - card_h - 12:
            raise LayoutError(f"{p.name}: too many areas for the left column")
        p.box(MARGIN, y, left_w, 86, kind)
        p.text(MARGIN + 16, y + 32, name, 13, WHITE, 700, fit=left_w - 150)
        x = MARGIN + 16
        if not idents:
            p.text(x, y + 62, "no open points", 9, DIM)
        for ident in idents:
            x += p.badge(x, y + 64, ident, fit=MARGIN + left_w - 16 - x) + 8
        counts = {}
        for ident in idents:
            priority = REGISTRY[ident].priority
            counts[priority] = counts.get(priority, 0) + 1
        summary = " · ".join(f"{n} {priority}" for priority, n in counts.items())
        p.text(MARGIN + left_w - 16, y + 32, summary, 9, SUB, anchor="end")
        y += 98

    card = p.card(MARGIN, p.bottom - card_h, left_w, card_h, "Suggested order", order_notes,
                  dot="#22d3ee")
    x = MARGIN + 16
    for i, ident in enumerate(order):
        width = p.badge(x, card.b - 18, ident)
        if i + 1 < len(order):
            p.text(x + width + 3, card.b - 19, "→", 9, DIM)
        x += width + 12
    if x > card.r:
        raise LayoutError(f"{p.name}: the suggested order is wider than its card")

    total_w = p.W - MARGIN - x0
    rest = total_w - 142
    widths = [60, 82, round(rest * 0.2), round(rest * 0.4)]
    widths.append(total_w - sum(widths))
    columns = list(zip(("ID", "priority", "where", "finding", "proposal"), widths))
    size, line_h, pad = 10.0, 14.0, 8.0
    p.rect(x0, p.top, total_w, 28, "rgba(30, 41, 59, 0.9)", stroke="#334155", width=1, rx=4)
    x = x0
    for title, w in columns:
        p.text(x + pad, p.top + 19, title, 10, WHITE, 700)
        x += w
    y = p.top + 30
    for i, item in enumerate(REGISTRY.values()):
        cells = [wrap(text, size, w - 2 * pad) for text, (_, w) in
                 zip((item.where, item.finding, item.proposal), columns[2:])]
        h = max(len(c) for c in cells) * line_h + 2 * pad
        fill = "rgba(15, 23, 42, 0.85)" if i % 2 == 0 else "rgba(30, 41, 59, 0.55)"
        p.rect(x0, y, total_w, h, fill, rx=3)
        p.badge(x0 + pad, y + pad + 11, item.ident)
        color = PRIORITIES[item.priority]
        p.circle(x0 + columns[0][1] + pad + 4, y + pad + 7, 4, color, layer="text")
        p.text(x0 + columns[0][1] + pad + 13, y + pad + 11, item.priority, size, color)
        x = x0 + columns[0][1] + columns[1][1]
        for (_, w), cell in zip(columns[2:], cells):
            ty = y + pad + 11
            for line in cell:
                p.text(x + pad, ty, line, size, SUB if w < 200 else TEXT, fit=w - 2 * pad)
                ty += line_h
            x += w
        y += h + 2
    if y > p.bottom - 15:
        raise LayoutError(f"{p.name}: the table ends at y={y:.0f}, past the bottom")
    counts = {priority: sum(1 for item in REGISTRY.values() if item.priority == priority)
              for priority in PRIORITIES}
    p.text(x0, y + 20, f"{len(REGISTRY)} points: " + ", ".join(
        f"{n} {priority}" for priority, n in counts.items()) + ".", 10, SUB, fit=total_w)


# ----------------------------------------------------------------------- build and print

class Spec:
    """One page to build: file name (without .pdf), title, subtitle and the function that
    draws it on a Page."""

    def __init__(self, name, title, subtitle, draw):
        self.name, self.title, self.subtitle, self.draw = name, title, subtitle, draw


def git(root, *args):
    command = ["git", "-c", f"safe.directory={Path(root).resolve().as_posix()}", "-C",
               str(root), *args]
    try:
        return subprocess.run(command, check=True, capture_output=True, text=True,
                              encoding="utf-8").stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return ""


def find_browser():
    """Edge or Chrome: $CHROME, the usual install folders, then PATH."""
    candidates = [os.environ.get("CHROME")]
    for base in ("PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"):
        if os.environ.get(base):
            candidates += [Path(os.environ[base]) / "Microsoft/Edge/Application/msedge.exe",
                           Path(os.environ[base]) / "Google/Chrome/Application/chrome.exe"]
    candidates += ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                   "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"]
    candidates += [shutil.which(name) for name in (
        "google-chrome", "google-chrome-stable", "chromium", "chromium-browser",
        "microsoft-edge", "msedge", "chrome")]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return str(candidate)
    sys.exit("no Chromium-based browser found (Edge or Chrome); set CHROME to its path")


DATE = re.compile(rb"/(CreationDate|ModDate) \(D:\d{14}")
PAGE_OBJECT = re.compile(rb"/Type\s*/Page(?![s\w])")


def print_pdf(browser, html_path, pdf_path, profile, stamp):
    """Print `html_path` to `pdf_path` with the headless browser, check it has one page, and
    pin its dates to `stamp` (14 digits: the same length, so cross-reference offsets hold)."""
    pdf_path.unlink(missing_ok=True)
    subprocess.run([browser, "--headless", "--disable-gpu", "--no-first-run",
                    "--no-default-browser-check", "--no-pdf-header-footer",
                    "--print-to-pdf-no-header", f"--user-data-dir={profile}",
                    f"--print-to-pdf={pdf_path}", Path(html_path).resolve().as_uri()],
                   check=True, capture_output=True, timeout=180)
    if not pdf_path.is_file() or pdf_path.stat().st_size == 0:
        raise RuntimeError(f"the browser wrote no {pdf_path.name}")
    data = DATE.sub(lambda m: b"/" + m.group(1) + b" (D:" + stamp, pdf_path.read_bytes())
    pages = len(PAGE_OBJECT.findall(data))
    if pages != 1:
        raise RuntimeError(f"{pdf_path.name} has {pages} pages, expected 1: something "
                           "reaches past the page's bottom edge")
    pdf_path.write_bytes(data)


def build(specs, out_dir, *, project, generator, footer_right="", size="A3", only=None,
          html_dir=None, root=None):
    """Lay out every page (all of them first, so a layout error stops the build before anything
    is printed), then print each to `out_dir/<name>.pdf`. Returns the PDF paths."""
    root = Path(root or Path.cwd())
    commit = git(root, "rev-parse", "--short=7", "HEAD") or "(not in git)"
    # Reproducible dates: SOURCE_DATE_EPOCH, else the last commit, else the generator's mtime.
    seconds = os.environ.get("SOURCE_DATE_EPOCH") or git(root, "log", "-1", "--format=%ct")
    if not seconds:
        script = Path(sys.argv[0])
        seconds = str(int(script.stat().st_mtime)) if script.is_file() else "0"
    when = datetime.fromtimestamp(int(seconds), timezone.utc)
    stamp = when.strftime("%Y%m%d%H%M%S").encode()

    pages = []
    for number, spec in enumerate(specs, 1):
        if only and only not in (f"{number:02d}", str(number), spec.name):
            continue
        page = Page(number, len(specs), spec.name, spec.title, spec.subtitle, size)
        try:
            spec.draw(page)
            page.frame(project, commit, when.strftime("%Y-%m-%d"), generator, footer_right)
        except LayoutError as error:
            sys.exit(f"layout error: {error}")
        pages.append(page)
    if not pages:
        sys.exit(f"no page matches {only!r}")

    browser = find_browser()
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    written = []
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as tmp:
        html_root = Path(html_dir) if html_dir else Path(tmp)
        html_root.mkdir(parents=True, exist_ok=True)
        for page in pages:
            html_path = html_root / f"{page.name}.html"
            html_path.write_bytes(page.html(project).encode("utf-8"))
            pdf_path = out_dir / f"{page.name}.pdf"
            print_pdf(browser, html_path, pdf_path, Path(tmp) / "profile", stamp)
            print(f"wrote {pdf_path} ({pdf_path.stat().st_size // 1024} KB)")
            written.append(pdf_path)
    return written


def main(specs, out_dir, **options):
    """Command line for a generator script: --only, --html, --out, --list."""
    parser = argparse.ArgumentParser(description="Draw the architecture diagrams as PDF.")
    parser.add_argument("--only", help="one page, by number (04) or name")
    parser.add_argument("--html", help="also keep the HTML pages in this folder")
    parser.add_argument("--out", help=f"output folder (default {out_dir})")
    parser.add_argument("--list", action="store_true", help="list the pages")
    args = parser.parse_args()
    if args.list:
        for number, spec in enumerate(specs, 1):
            print(f"{number:02d}  {spec.name}  {spec.title}")
        return
    build(specs, args.out or out_dir, only=args.only, html_dir=args.html, **options)
