"""Public Photoshop enum values used by the Rust bindings."""

from enum import IntEnum


class BitDepth(IntEnum):
    bd_1 = 0
    bd_8 = 1
    bd_16 = 2
    bd_32 = 3


class ColorMode(IntEnum):
    grayscale = 1
    rgb = 3
    cmyk = 4


class Compression(IntEnum):
    raw = 0
    rle = 1
    zip = 2
    zipprediction = 3


class ChannelID(IntEnum):
    red = 0
    green = 1
    blue = 2
    cyan = 3
    magenta = 4
    yellow = 5
    black = 6
    gray = 7
    custom = 8
    alpha = 9
    mask = 10


class BlendMode(IntEnum):
    """Layer blend modes, in upstream's order. ``passthrough`` is for groups."""

    passthrough = 0
    normal = 1
    dissolve = 2
    darken = 3
    multiply = 4
    colorburn = 5
    linearburn = 6
    darkercolor = 7
    lighten = 8
    screen = 9
    colordodge = 10
    lineardodge = 11
    lightercolor = 12
    overlay = 13
    softlight = 14
    hardlight = 15
    vividlight = 16
    linearlight = 17
    pinlight = 18
    hardmix = 19
    difference = 20
    exclusion = 21
    subtract = 22
    divide = 23
    hue = 24
    saturation = 25
    color = 26
    luminosity = 27


class LayerColor(IntEnum):
    """Layer display colors in the layers panel (upstream spells ``fuschia``)."""

    none = 0
    red = 1
    orange = 2
    yellow = 3
    green = 4
    blue = 5
    violet = 6
    gray = 7
    seafoam = 8
    indigo = 9
    magenta = 10
    fuschia = 11


class LinkedLayerType(IntEnum):
    data = 0
    external = 1


class WritingDirection(IntEnum):
    Horizontal = 0
    Vertical = 2


class ShapeType(IntEnum):
    PointText = 0
    BoxText = 1


class WarpStyle(IntEnum):
    NoWarp = 0
    Arc = 1
    ArcLower = 2
    ArcUpper = 3
    Arch = 4
    Bulge = 5
    ShellLower = 6
    ShellUpper = 7
    Flag = 8
    Wave = 9
    Fish = 10
    Rise = 11
    FishEye = 12
    Inflate = 13
    Squeeze = 14
    Twist = 15
    Custom = 16


class WarpRotation(IntEnum):
    Horizontal = 0
    Vertical = 1


class FontType(IntEnum):
    OpenType = 0
    TrueType = 1


class FontScript(IntEnum):
    Roman = 0
    CJK = 1


class FontCaps(IntEnum):
    Normal = 0
    SmallCaps = 1
    AllCaps = 2


class FontBaseline(IntEnum):
    Normal = 0
    Superscript = 1
    Subscript = 2


class CharacterDirection(IntEnum):
    Default = 0
    LeftToRight = 1
    RightToLeft = 2


class BaselineDirection(IntEnum):
    Default = 0
    Vertical = 1
    CrossStream = 2


class DiacriticPosition(IntEnum):
    OpenType = 0
    Loose = 1
    Medium = 2
    Tight = 3


class Justification(IntEnum):
    Left = 0
    Right = 1
    Center = 2
    JustifyLastLeft = 3
    JustifyLastRight = 4
    JustifyLastCenter = 5
    JustifyAll = 6


class LeadingType(IntEnum):
    BottomToBottom = 0
    TopToTop = 1


class KinsokuOrder(IntEnum):
    PushInFirst = 0
    PushOutFirst = 1


class AntiAliasMethod(IntEnum):
    NoAntiAlias = 0
    Crisp = 1
    Strong = 2
    Smooth = 3
    Sharp = 4


__all__ = [
    "BitDepth",
    "ColorMode",
    "Compression",
    "ChannelID",
    "BlendMode",
    "LayerColor",
    "LinkedLayerType",
    "WritingDirection", "ShapeType", "WarpStyle", "WarpRotation",
    "FontType", "FontScript", "FontCaps", "FontBaseline",
    "CharacterDirection", "BaselineDirection", "DiacriticPosition",
    "Justification", "LeadingType", "KinsokuOrder", "AntiAliasMethod",
]
