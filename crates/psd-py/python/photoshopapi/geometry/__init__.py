"""Geometry types and helpers exposed by the PhotoshopAPI binding."""

from dataclasses import dataclass
from math import hypot, isfinite
from numbers import Real

from .._native import homography_matrix


@dataclass
class Point2D:
    # Explicit slots: `dataclass(slots=True)` needs Python 3.10, and the
    # package supports 3.9.
    __slots__ = ("x", "y")

    x: float
    y: float

    def __post_init__(self):
        self.x = float(self.x)
        self.y = float(self.y)

    @staticmethod
    def _components(other):
        if isinstance(other, Point2D):
            return other.x, other.y
        if isinstance(other, Real):
            value = float(other)
            return value, value
        return NotImplemented

    def __add__(self, other):
        pair = self._components(other)
        if pair is NotImplemented:
            return NotImplemented
        return Point2D(self.x + pair[0], self.y + pair[1])

    def __sub__(self, other):
        pair = self._components(other)
        if pair is NotImplemented:
            return NotImplemented
        return Point2D(self.x - pair[0], self.y - pair[1])

    def __mul__(self, other):
        pair = self._components(other)
        if pair is NotImplemented:
            return NotImplemented
        return Point2D(self.x * pair[0], self.y * pair[1])

    def __truediv__(self, other):
        pair = self._components(other)
        if pair is NotImplemented:
            return NotImplemented
        if pair[0] == 0 or pair[1] == 0:
            raise ValueError("cannot divide Point2D by zero")
        return Point2D(self.x / pair[0], self.y / pair[1])

    def __neg__(self):
        return Point2D(-self.x, -self.y)

    def __len__(self):
        return 2

    def __iter__(self):
        yield self.x
        yield self.y

    def __hash__(self):
        return hash((self.x, self.y))

    def __repr__(self):
        return f"[{self.x}, {self.y}]"

    def distance(self, other):
        if not isinstance(other, Point2D):
            raise TypeError("distance expects another Point2D")
        return hypot(self.x - other.x, self.y - other.y)

    @staticmethod
    def lerp(a, b, t):
        if not 0 <= t <= 1:
            raise ValueError("t must be between 0 and 1")
        return Point2D(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)


def create_quad(width, height):
    width = float(width)
    height = float(height)
    if not isfinite(width) or not isfinite(height):
        raise ValueError("quad dimensions must be finite")
    return [Point2D(0, 0), Point2D(width, 0), Point2D(0, height), Point2D(width, height)]


def create_normalized_quad():
    return create_quad(1, 1)


def create_homography(source_points, destination_points):
    if len(source_points) != 4 or len(destination_points) != 4:
        raise ValueError("both quads must have four points")
    return homography_matrix(
        [(point.x, point.y) for point in source_points],
        [(point.x, point.y) for point in destination_points],
    )


__all__ = ["Point2D", "create_quad", "create_normalized_quad", "create_homography"]
