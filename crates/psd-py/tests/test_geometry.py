"""Geometry binding contracts mirrored from upstream Python tests."""

import unittest

import numpy as np

from photoshopapi.geometry import (
    Point2D,
    create_homography,
    create_normalized_quad,
    create_quad,
)


class GeometryBindingsTest(unittest.TestCase):
    def test_point_arithmetic_lerp_and_distance(self):
        a = Point2D(1, 1)
        b = Point2D(2, 2)
        self.assertEqual(a + b, Point2D(3, 3))
        self.assertEqual(a + 2.0, Point2D(3, 3))
        self.assertEqual(a - b, Point2D(-1, -1))
        self.assertEqual(a * b, Point2D(2, 2))
        self.assertEqual(a / b, Point2D(0.5, 0.5))
        self.assertEqual(-a, Point2D(-1, -1))
        self.assertEqual(Point2D.lerp(a, Point2D(3, 3), 0.5), b)
        self.assertEqual(a.distance(Point2D(3, 1)), 2.0)
        with self.assertRaises(ValueError):
            a / 0

    def test_quad_and_homography_use_double_precision(self):
        self.assertEqual(create_normalized_quad()[3], Point2D(1, 1))
        source = create_quad(50, 25)
        destination = create_quad(50, 25)
        destination[0] = Point2D(-5, -10)
        matrix = create_homography(source, destination)
        self.assertEqual(matrix.shape, (3, 3))
        self.assertEqual(matrix.dtype, np.float64)
        for point, expected in zip(source, destination):
            transformed = matrix @ np.array([point.x, point.y, 1.0])
            # Double-precision round-off only (zero-valued coordinates need an
            # absolute tolerance).
            np.testing.assert_allclose(transformed[:2] / transformed[2], tuple(expected), atol=1e-9)


if __name__ == "__main__":
    unittest.main()
