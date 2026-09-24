"""Text-layer style API generated from the native property tables.

Upstream binds every character and paragraph property as flat methods
(``style_run_font_size(i)``, ``set_style_normal_leading(v)``, ...), as proxy
objects (``layer.style_run(i).font_size``), and as range setters
(``layer.style_text("Bold").set_bold(True)``). The native extension exposes
one generic accessor per sheet kind; this module builds that whole surface
from the property-name tables so the Rust side stays a single table.
"""

from . import _native
from .enum import FontScript, FontType

CHARACTER_PROPERTIES, PARAGRAPH_PROPERTIES = _native.text_property_names()


def _function(name, doc, body):
    body.__name__ = name
    body.__qualname__ = name
    body.__doc__ = doc
    return body


def _flat_methods(prefix, names, getter, setter):
    """``{prefix}_{name}`` getters and ``set_{prefix}_{name}`` setters."""
    methods = {}
    sheet = "run" if prefix.endswith("_run") else "normal"
    for name in names:
        if sheet == "run":
            def get(self, run_index, _name=name):
                return getattr(self, getter)("run", run_index, _name)

            def set_(self, run_index, value, _name=name):
                getattr(self, setter)("run", run_index, _name, value)
        else:
            def get(self, _name=name):
                return getattr(self, getter)("normal", 0, _name)

            def set_(self, value, _name=name):
                getattr(self, setter)("normal", 0, _name, value)

        methods[f"{prefix}_{name}"] = _function(
            f"{prefix}_{name}",
            f"``{name}`` of the {prefix.replace('_', ' ')} (``None`` when absent).",
            get,
        )
        methods[f"set_{prefix}_{name}"] = _function(
            f"set_{prefix}_{name}",
            f"Set ``{name}`` of the {prefix.replace('_', ' ')}; raises ``ValueError`` "
            "for bad indices or values.",
            set_,
        )
    return methods


class _SheetProxy:
    """A live view of one style or paragraph sheet of a text layer."""

    __slots__ = ("_layer", "_index")
    _kind = "run"
    _getter = "_style"
    _setter = "_set_style"

    def __init__(self, layer, index=0):
        self._layer = layer
        self._index = index

    def __repr__(self):
        return f"{type(self).__name__}(index={self._index})"


def _proxy_class(class_name, names, getter, setter, kind, doc, extra=None):
    namespace = {"__slots__": (), "__doc__": doc, "_kind": kind}
    for name in names:
        def get(self, _name=name):
            return getattr(self._layer, getter)(self._kind, self._index, _name)

        def set_(self, value, _name=name):
            getattr(self._layer, setter)(self._kind, self._index, _name, value)

        namespace[name] = property(get, doc=f"``{name}`` (``None`` when absent).")
        namespace[f"set_{name}"] = _function(f"set_{name}", f"Set ``{name}``.", set_)
    namespace.update(extra or {})
    return type(class_name, (_SheetProxy,), namespace)


StyleRunProxy = _proxy_class(
    "StyleRunProxy",
    CHARACTER_PROPERTIES,
    "_style",
    "_set_style",
    "run",
    "Character style of one style run (``layer.style_run(i)``).",
    {
        "set_font_by_name": lambda self, postscript_name: self._layer.set_style_run_font_by_name(
            self._index, postscript_name
        ),
    },
)

StyleNormalProxy = _proxy_class(
    "StyleNormalProxy",
    CHARACTER_PROPERTIES,
    "_style",
    "_set_style",
    "normal",
    "The normal (default) character style sheet (``layer.style_normal()``).",
    {
        "sheet_index": property(lambda self: self._layer.style_normal_sheet_index()),
        "set_sheet_index": lambda self, sheet_index: self._layer.set_style_normal_sheet_index(
            sheet_index
        ),
        "set_font_by_name": lambda self, postscript_name: self._layer.set_style_normal_font_by_name(
            postscript_name
        ),
    },
)

ParagraphRunProxy = _proxy_class(
    "ParagraphRunProxy",
    PARAGRAPH_PROPERTIES,
    "_paragraph",
    "_set_paragraph",
    "run",
    "Paragraph style of one paragraph run (``layer.paragraph_run(i)``).",
)

ParagraphNormalProxy = _proxy_class(
    "ParagraphNormalProxy",
    PARAGRAPH_PROPERTIES,
    "_paragraph",
    "_set_paragraph",
    "normal",
    "The normal (default) paragraph sheet (``layer.paragraph_normal()``).",
    {
        "sheet_index": property(lambda self: self._layer.paragraph_normal_sheet_index()),
        "set_sheet_index": lambda self, sheet_index: self._layer.set_paragraph_normal_sheet_index(
            sheet_index
        ),
    },
)


class FontProxy:
    """One entry of a text layer's FontSet (``layer.font(i)``)."""

    __slots__ = ("_layer", "_index")

    def __init__(self, layer, index):
        self._layer = layer
        self._index = index

    @property
    def postscript_name(self):
        return self._layer.font_postscript_name(self._index)

    @property
    def name(self):
        return self._layer.font_name(self._index)

    @property
    def script(self):
        return self._layer.font_script(self._index)

    @property
    def type(self):
        return self._layer.font_type(self._index)

    @property
    def synthetic(self):
        return self._layer.font_synthetic(self._index)

    @property
    def is_sentinel(self):
        return self._layer.is_sentinel_font(self._index)

    def set_postscript_name(self, name):
        self._layer.set_font_postscript_name(self._index, name)

    def __repr__(self):
        return f"FontProxy({self.postscript_name!r})"


class FontSetProxy:
    """A text layer's FontSet (``layer.font_set()``)."""

    __slots__ = ("_layer",)

    def __init__(self, layer):
        self._layer = layer

    @property
    def count(self):
        return self._layer.font_count

    @property
    def used_indices(self):
        return self._layer.used_font_indices()

    @property
    def used_names(self):
        return self._layer.used_font_names()

    def find_index(self, postscript_name):
        return self._layer.find_font_index(postscript_name)

    def add(self, postscript_name, font_type=FontType.OpenType, script=FontScript.Roman, synthetic=0):
        return self._layer.add_font(postscript_name, font_type, script, synthetic)

    def __len__(self):
        return self.count


class _RangeProxy:
    """Chainable setters over the characters a range covers. Setters on a
    range that matched nothing are no-ops."""

    __slots__ = ("_layer", "_spec")
    _paragraph = False

    def __init__(self, layer, spec):
        self._layer = layer
        self._spec = spec

    def range_count(self):
        """Number of character ranges this proxy covers."""
        return self._layer._range_count(self._spec, self._paragraph)

    def valid(self):
        """True if this proxy targets at least one character range."""
        return self.range_count() > 0

    def __repr__(self):
        return f"{type(self).__name__}{self._spec!r}"


def _range_class(class_name, names, native, paragraph, doc, extra=None):
    namespace = {"__slots__": (), "__doc__": doc, "_paragraph": paragraph}
    for name in names:
        def set_(self, value, _name=name):
            getattr(self._layer, native)(self._spec, _name, value)
            return self

        namespace[f"set_{name}"] = _function(
            f"set_{name}", f"Set ``{name}`` on every covered run; returns the range.", set_
        )
    namespace.update(extra or {})
    return type(class_name, (_RangeProxy,), namespace)


def _range_setter(name, native_name, doc):
    def set_(self, value):
        self._layer._style_range(self._spec, native_name, value)
        return self

    return _function(name, doc, set_)


CharacterStyleRange = _range_class(
    "CharacterStyleRange",
    CHARACTER_PROPERTIES,
    "_style_range",
    False,
    "Character styles for a range of text (``style_range``/``style_text``/``style_all``).",
    {
        "set_bold": _range_setter("set_bold", "faux_bold", "Enable/disable faux bold."),
        "set_italic": _range_setter("set_italic", "faux_italic", "Enable/disable faux italic."),
        "set_font": _range_setter(
            "set_font", "font_name", "Use the font with this PostScript name (added if needed)."
        ),
        "set_font_index": _range_setter("set_font_index", "font", "Use FontSet entry ``index``."),
    },
)

ParagraphStyleRange = _range_class(
    "ParagraphStyleRange",
    PARAGRAPH_PROPERTIES,
    "_paragraph_range",
    True,
    "Paragraph styles for the paragraphs a range touches "
    "(``paragraph_range``/``paragraph_text``/``paragraph_all``).",
)


def _text_methods():
    methods = {}
    methods.update(_flat_methods("style_run", CHARACTER_PROPERTIES, "_style", "_set_style"))
    methods.update(_flat_methods("style_normal", CHARACTER_PROPERTIES, "_style", "_set_style"))
    methods.update(
        _flat_methods("paragraph_run", PARAGRAPH_PROPERTIES, "_paragraph", "_set_paragraph")
    )
    methods.update(
        _flat_methods("paragraph_normal", PARAGRAPH_PROPERTIES, "_paragraph", "_set_paragraph")
    )

    def style_run(self, run_index):
        """A live proxy for style run ``run_index``."""
        return StyleRunProxy(self, run_index)

    def style_normal(self):
        """A live proxy for the normal character style sheet."""
        return StyleNormalProxy(self)

    def paragraph_run(self, run_index):
        """A live proxy for paragraph run ``run_index``."""
        return ParagraphRunProxy(self, run_index)

    def paragraph_normal(self):
        """A live proxy for the normal paragraph sheet."""
        return ParagraphNormalProxy(self)

    def font(self, font_index):
        """A proxy for FontSet entry ``font_index``."""
        return FontProxy(self, font_index)

    def font_set(self):
        """A proxy for the whole FontSet."""
        return FontSetProxy(self)

    def style_range(self, start, end):
        """Character styles for UTF-16 code units ``[start, end)``."""
        return CharacterStyleRange(self, ("range", start, end))

    def style_text(self, needle, occurrence=0):
        """Character styles for ``needle`` (0 = every occurrence, n = the n-th)."""
        return CharacterStyleRange(self, ("text", needle, occurrence))

    def style_all(self):
        """Character styles for the whole text."""
        return CharacterStyleRange(self, ("all",))

    def paragraph_range(self, start, end):
        """Paragraph styles for the paragraphs ``[start, end)`` touches."""
        return ParagraphStyleRange(self, ("range", start, end))

    def paragraph_text(self, needle, occurrence=0):
        """Paragraph styles for the paragraphs containing ``needle``."""
        return ParagraphStyleRange(self, ("text", needle, occurrence))

    def paragraph_all(self):
        """Paragraph styles for every paragraph."""
        return ParagraphStyleRange(self, ("all",))

    for function in (
        style_run, style_normal, paragraph_run, paragraph_normal, font, font_set,
        style_range, style_text, style_all, paragraph_range, paragraph_text, paragraph_all,
    ):
        methods[function.__name__] = function
    return methods


def install(*classes):
    """Attach the generated methods to the native text-layer classes."""
    methods = _text_methods()
    for cls in classes:
        for name, function in methods.items():
            setattr(cls, name, function)


__all__ = [
    "StyleRunProxy", "StyleNormalProxy", "ParagraphRunProxy", "ParagraphNormalProxy",
    "FontProxy", "FontSetProxy", "CharacterStyleRange", "ParagraphStyleRange",
    "CHARACTER_PROPERTIES", "PARAGRAPH_PROPERTIES", "install",
]
