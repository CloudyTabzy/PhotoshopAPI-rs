"""Python interface to the Rust PhotoshopAPI port.

The API mirrors upstream PhotoshopAPI's pybind11 bindings: depth-specific
``LayeredFile_*bit`` documents and ``*Layer_*bit`` layer classes, plus the
``enum``, ``geometry``, and ``util`` modules. ``LayeredFile.read`` picks the
right depth automatically.
"""

from . import _native
from ._native import (
    GroupLayer_8bit,
    GroupLayer_16bit,
    GroupLayer_32bit,
    ImageLayer_8bit,
    ImageLayer_16bit,
    ImageLayer_32bit,
    Layer_8bit,
    Layer_16bit,
    Layer_32bit,
    LayeredFile_8bit,
    LayeredFile_16bit,
    LayeredFile_32bit,
    PhotoshopFile,
    SmartObjectLayer_8bit,
    SmartObjectLayer_16bit,
    SmartObjectLayer_32bit,
    SmartObjectWarp,
    TextLayer_8bit,
    TextLayer_16bit,
    TextLayer_32bit,
    read,
    rust_version,
)
from . import enum, geometry, util
from . import _text
from ._text import (
    CharacterStyleRange,
    FontProxy,
    FontSetProxy,
    ParagraphNormalProxy,
    ParagraphRunProxy,
    ParagraphStyleRange,
    StyleNormalProxy,
    StyleRunProxy,
)

_text.install(TextLayer_8bit, TextLayer_16bit, TextLayer_32bit)

# Upstream declares one proxy class per bit depth; the Python proxies are
# depth independent, so the per-depth names are aliases.
for _suffix in ("_8bit", "_16bit", "_32bit"):
    for _proxy in (
        CharacterStyleRange, FontProxy, FontSetProxy, ParagraphNormalProxy,
        ParagraphRunProxy, ParagraphStyleRange, StyleNormalProxy, StyleRunProxy,
    ):
        globals()[_proxy.__name__ + _suffix] = _proxy
del _suffix, _proxy


class LayeredFile:
    """Depth independent reader: ``LayeredFile.read(path)`` returns the
    ``LayeredFile_8bit``/``_16bit``/``_32bit`` matching the file."""

    read = staticmethod(read)


__all__ = [
    "LayeredFile", "LayeredFile_8bit", "LayeredFile_16bit", "LayeredFile_32bit",
    "Layer_8bit", "Layer_16bit", "Layer_32bit",
    "ImageLayer_8bit", "ImageLayer_16bit", "ImageLayer_32bit",
    "GroupLayer_8bit", "GroupLayer_16bit", "GroupLayer_32bit",
    "TextLayer_8bit", "TextLayer_16bit", "TextLayer_32bit",
    "SmartObjectLayer_8bit", "SmartObjectLayer_16bit", "SmartObjectLayer_32bit",
    "SmartObjectWarp", "PhotoshopFile",
    "StyleRunProxy", "StyleNormalProxy", "ParagraphRunProxy", "ParagraphNormalProxy",
    "FontProxy", "FontSetProxy", "CharacterStyleRange", "ParagraphStyleRange",
    "enum", "geometry", "util", "rust_version",
]
