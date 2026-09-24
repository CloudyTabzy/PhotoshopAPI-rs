"""Python integration checks for the native binding and NumPy ownership."""

import tempfile
import unittest
from pathlib import Path

import numpy as np

import photoshopapi as psapi


class DocumentBindingsTest(unittest.TestCase):
    def test_icc_numpy_and_metadata_roundtrip(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 3, 2)
        document.icc = np.array([1, 2, 3, 4], dtype=np.uint8)
        document.dpi = 144.0
        document.width = 4
        document.height = 3
        np.testing.assert_array_equal(document.icc, [1, 2, 3, 4])
        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        self.assertEqual((reread.width, reread.height), (4, 3))
        self.assertAlmostEqual(reread.dpi, 144.0)
        np.testing.assert_array_equal(reread.icc, [1, 2, 3, 4])
        with self.assertRaises(TypeError):
            document.icc = None

    def test_live_group_text_and_byte_roundtrip(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 16, 16)
        group = psapi.GroupLayer_8bit("Group")
        document.add_layer(group)
        text = psapi.TextLayer_8bit("Title", "Hello")
        group.add_layer(text)

        self.assertIsInstance(document["Group"], psapi.GroupLayer_8bit)
        self.assertIsInstance(document["Group"]["Title"], psapi.TextLayer_8bit)
        text.set_text("World")
        self.assertEqual(document.find_layer("Group/Title").text, "World")
        self.assertEqual(len(document.layers), 1)

        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        self.assertEqual(reread["Group"]["Title"].text, "World")

    def test_depth_dispatch_and_unicode_file_path(self):
        image = np.ones((3, 2, 3), dtype=np.uint16)
        document = psapi.LayeredFile_16bit(psapi.enum.ColorMode.rgb, 3, 2)
        document.add_layer(psapi.ImageLayer_16bit(image, "Pixels", width=3, height=2))

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "你好.psd"
            document.write(path)
            reread = psapi.LayeredFile.read(path)
            self.assertIsInstance(reread, psapi.LayeredFile_16bit)
            self.assertEqual(reread["Pixels"].name, "Pixels")


class ImageBindingsTest(unittest.TestCase):
    def test_numpy_channels_across_depths_and_live_mutation(self):
        cases = (
            (np.uint8, psapi.LayeredFile_8bit, psapi.ImageLayer_8bit),
            (np.uint16, psapi.LayeredFile_16bit, psapi.ImageLayer_16bit),
            (np.float32, psapi.LayeredFile_32bit, psapi.ImageLayer_32bit),
        )
        for dtype, document_type, image_type in cases:
            with self.subTest(dtype=dtype):
                pixels = np.arange(24, dtype=dtype).reshape(4, 2, 3)
                layer = image_type(pixels, "Pixels", width=3, height=2)
                self.assertEqual(layer.channel_indices(), [-1, 0, 1, 2])
                np.testing.assert_array_equal(layer[0], pixels[0])
                np.testing.assert_array_equal(layer[-1], pixels[3])
                self.assertEqual(layer[0].dtype, dtype)

                document = document_type(psapi.enum.ColorMode.rgb, 3, 2)
                document.add_layer(layer)
                layer[0] = np.full((2, 3), 7, dtype=dtype)
                self.assertTrue(np.all(document["Pixels"][0] == 7))
                reread = document_type.from_bytes(document.to_bytes())
                self.assertTrue(np.all(reread["Pixels"][0] == 7))

    def test_channel_dictionary_mask_and_noncontiguous_copy(self):
        source = np.arange(24, dtype=np.uint8).reshape(4, 2, 3)[:, ::-1, :]
        layer = psapi.ImageLayer_8bit(source, "Flipped", width=3, height=2)
        np.testing.assert_array_equal(layer[1], source[1])

        channels = {0: source[0], 1: source[1], 2: source[2], -1: source[3]}
        mask = np.full((2, 3), 255, dtype=np.uint8)
        channels[-2] = mask
        masked = psapi.ImageLayer_8bit(channels, "Masked", width=3, height=2)
        self.assertEqual(masked.num_channels(), 5)
        np.testing.assert_array_equal(masked.get_image_data()[-2], mask)

        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 3, 2)
        document.add_layer(masked)
        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        np.testing.assert_array_equal(reread["Masked"][-2], mask)

    def test_rejects_wrong_shape_and_color_channel(self):
        pixels = np.ones((4, 2, 3), dtype=np.uint8)
        with self.assertRaises(ValueError):
            psapi.ImageLayer_8bit(pixels, "Wrong", width=4, height=2)
        layer = psapi.ImageLayer_8bit(pixels, "Pixels", width=3, height=2)
        with self.assertRaises(RuntimeError):
            layer.set_channel_by_id(psapi.enum.ChannelID.cyan, pixels[0])
        with self.assertRaises(RuntimeError):
            layer.set_image_data({0: pixels[0], 1: pixels[1], -1: pixels[3]})


if __name__ == "__main__":
    unittest.main()
