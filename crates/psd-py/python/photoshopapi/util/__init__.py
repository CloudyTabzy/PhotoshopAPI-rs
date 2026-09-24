"""Small utility types exposed by the PhotoshopAPI binding."""

import os
from dataclasses import dataclass

from ..enum import ChannelID, ColorMode

_PER_MODE = {
    ColorMode.rgb: {ChannelID.red: 0, ChannelID.green: 1, ChannelID.blue: 2},
    ColorMode.cmyk: {
        ChannelID.cyan: 0, ChannelID.magenta: 1, ChannelID.yellow: 2, ChannelID.black: 3,
    },
    ColorMode.grayscale: {ChannelID.gray: 0},
}
_SPECIAL = {ChannelID.alpha: -1, ChannelID.mask: -2}


@dataclass
class ChannelIDInfo:
    """A channel's ID together with its logical index (upstream
    ``Enum::ChannelIDInfo``). Custom channels use ``ChannelID.custom`` with
    their index."""

    id: ChannelID = ChannelID.custom
    index: int = 0

    @classmethod
    def from_id(cls, channel_id, color_mode):
        """The info for a channel ID in a color mode."""
        channel_id = ChannelID(channel_id)
        color_mode = ColorMode(color_mode)
        if channel_id in _SPECIAL:
            return cls(channel_id, _SPECIAL[channel_id])
        try:
            return cls(channel_id, _PER_MODE[color_mode][channel_id])
        except KeyError as error:
            raise ValueError("channel ID is invalid for this color mode") from error

    @classmethod
    def from_index(cls, index, color_mode):
        """The info for a channel index in a color mode; indices past the
        color channels are custom channels."""
        color_mode = ColorMode(color_mode)
        for channel_id, special in _SPECIAL.items():
            if special == index:
                return cls(channel_id, index)
        for channel_id, known in _PER_MODE[color_mode].items():
            if known == index:
                return cls(channel_id, index)
        if index < 0:
            raise ValueError(f"channel index {index} is invalid")
        return cls(ChannelID.custom, index)

    def set_id(self, channel_id, color_mode):
        """Change the ID, updating the index to match (upstream's ``id``
        setter takes the color mode too)."""
        info = self.from_id(channel_id, color_mode)
        self.id, self.index = info.id, info.index

    def set_index(self, index, color_mode):
        """Change the index, updating the ID to match."""
        info = self.from_index(index, color_mode)
        self.id, self.index = info.id, info.index


class File:
    """A file path for ``PhotoshopFile.read``/``write`` (upstream's opaque
    ``File`` handle)."""

    def __init__(self, path):
        self.path = os.fspath(path)

    def __fspath__(self):
        return self.path

    def __repr__(self):
        return f"File({self.path!r})"


__all__ = ["ChannelIDInfo", "File"]
