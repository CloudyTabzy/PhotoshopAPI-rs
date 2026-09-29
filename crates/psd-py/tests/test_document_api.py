"""Document structure, layer settings, text ranges, and I/O through Python.

Covers the binding surface the ported upstream suite (``tests/upstream``)
does not exercise: tree edits, detached groups, per-layer settings, range
proxies, stream I/O, ``PhotoshopFile``, and group masks.
"""

import inspect
import io
import os
import struct
import tempfile
import unittest
from pathlib import Path

import numpy as np

import photoshopapi as psapi

FIXTURES = Path(__file__).resolve().parents[3] / "fixtures"


def pixels(value=100, size=8):
    return np.full((3, size, size), value, dtype=np.uint8)


def image(name, value=100, size=8):
    return psapi.ImageLayer_8bit(pixels(value, size), name, pos_x=size / 2, pos_y=size / 2)


def names(layers):
    return [layer.name for layer in layers]


def roundtrip(document):
    return type(document).from_bytes(document.to_bytes())


class TreeTest(unittest.TestCase):
    def test_upstream_ordering_first_added_is_on_top(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 8, 8)
        document.add_layer(image("Top"))
        document.add_layer(image("Bottom"))
        self.assertEqual(names(document.layers), ["Top", "Bottom"])
        self.assertEqual(names(roundtrip(document).layers), ["Top", "Bottom"])

    def test_detached_group_tree_attaches_with_live_children(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 8, 8)
        group = psapi.GroupLayer_8bit("Group")
        inner = psapi.GroupLayer_8bit("Inner")
        leaf = image("Leaf")
        inner.add_layer(leaf)
        group.add_layer(document, inner)
        leaf.name = "Renamed"
        self.assertEqual(names(group["Inner"].layers), ["Renamed"])
        self.assertFalse(document.is_layer_in_document(leaf))

        document.add_layer(group)
        self.assertTrue(document.is_layer_in_document(leaf))
        self.assertEqual(document.find_layer("Group/Inner/Renamed"), leaf)
        self.assertEqual(names(document.flat_layers), ["Group", "Inner", "Renamed"])
        reread = roundtrip(document)
        self.assertEqual(names(reread.flat_layers), ["Group", "Inner", "Renamed"])
        self.assertEqual(reread["Group"].blend_mode, psapi.enum.BlendMode.passthrough)

    def test_move_remove_and_re_add(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 8, 8)
        document.add_layer(image("A"))
        group = psapi.GroupLayer_8bit("Group")
        document.add_layer(group)
        group.add_layer(image("Child"))

        document.move_layer("Group/Child")
        self.assertEqual(names(document.layers), ["A", "Group", "Child"])
        document.move_layer(document["Child"], group)
        self.assertEqual(names(group.layers), ["Child"])
        with self.assertRaises(ValueError):
            document.move_layer(group, "Group/Child")

        document.remove_layer(group)
        self.assertFalse(document.is_layer_in_document(group))
        self.assertEqual(names(document.layers), ["A"])
        self.assertEqual(names(group.layers), ["Child"])
        document.add_layer(group)
        self.assertEqual(names(document.flat_layers), ["A", "Group", "Child"])
        with self.assertRaises(ValueError):
            document.add_layer(group)

        group.remove_layer("Child")
        self.assertEqual(len(group), 0)
        document.remove_layer("A")
        self.assertEqual(names(roundtrip(document).flat_layers), ["Group"])

    def test_detached_group_remove_by_index_and_object(self):
        group = psapi.GroupLayer_8bit("Group")
        first, second = image("First"), image("Second")
        group.add_layer(first)
        group.add_layer(second)
        self.assertEqual(names(group.layers), ["First", "Second"])
        group.remove_layer(1)
        self.assertEqual(names(group.layers), ["First"])
        group.remove_layer(group.layers[0])
        self.assertEqual(len(group), 0)
        with self.assertRaises(IndexError):
            group.remove_layer(0)

    def test_equality_follows_the_layer_not_the_wrapper(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 8, 8)
        document.add_layer(image("A"))
        document.add_layer(image("B"))
        self.assertEqual(document["A"], document.layers[0])
        self.assertNotEqual(document["A"], document["B"])
        self.assertEqual(len({document["A"], document.find_layer("A")}), 1)
        self.assertEqual(repr(document["A"]), 'ImageLayer_8bit("A")')


class SettingsTest(unittest.TestCase):
    def test_layer_settings_round_trip(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 8, 8)
        document.add_layer(image("Layer"))
        layer = document["Layer"]
        layer.fill = 0.5
        layer.is_locked = True
        layer.display_color = psapi.enum.LayerColor.red
        layer.clipping_mask = True
        layer.blend_mode = psapi.enum.BlendMode.multiply
        layer.is_visible = 0
        layer.mask = np.full((4, 6), 128, np.uint8)
        layer.mask_density = 200
        layer.mask_feather = 1.5
        layer.mask_disabled = True
        layer.set_mask_compression(psapi.enum.Compression.rle)

        back = roundtrip(document)["Layer"]
        self.assertAlmostEqual(back.fill, 128 / 255)
        self.assertTrue(back.is_locked)
        self.assertEqual(back.display_color, psapi.enum.LayerColor.red)
        self.assertTrue(back.clipping_mask)
        self.assertEqual(back.blend_mode, psapi.enum.BlendMode.multiply)
        self.assertFalse(back.is_visible)
        self.assertEqual(back.mask.shape, (4, 6))
        self.assertEqual((back.mask_density, back.mask_feather), (200, 1.5))
        self.assertTrue(back.mask_disabled)
        self.assertEqual(back.mask_position, psapi.geometry.Point2D(3, 2))

    def test_opacity_is_clamped_with_a_warning(self):
        layer = image("Layer")
        with self.assertWarns(UserWarning):
            layer.opacity = 1.5
        self.assertEqual(layer.opacity, 1.0)
        with self.assertRaises(ValueError):
            psapi.ImageLayer_8bit(pixels(), "Layer", opacity=2.0)

    def test_mask_settings_need_a_mask(self):
        layer = image("Layer")
        self.assertFalse(layer.has_mask())
        self.assertEqual(layer.mask.size, 0)
        self.assertEqual(layer.mask_position, psapi.geometry.Point2D(-1, -1))
        with self.assertRaises(ValueError):
            layer.mask_density = 10

    def test_center_and_size_follow_upstream(self):
        layer = psapi.ImageLayer_8bit(pixels(size=4), "Layer", pos_x=10, pos_y=20)
        self.assertEqual((layer.center_x, layer.center_y), (10.0, 20.0))
        layer.center_x = 30
        self.assertEqual(layer.center_x, 30.0)
        # Resizing keeps the center; the channels are replaced afterwards.
        layer.width = 8
        layer.height = 8
        layer.set_image_data(pixels(size=8))
        self.assertEqual((layer.width, layer.height, layer.center_x), (8, 8, 30.0))

    def test_compression_setter_is_write_only(self):
        document = psapi.LayeredFile_16bit(psapi.enum.ColorMode.rgb, 4, 4)
        document.compression = psapi.enum.Compression.zip
        with self.assertRaises(TypeError):
            document.compression
        self.assertEqual(document.bit_depth, psapi.enum.BitDepth.bd_16)
        self.assertEqual(document.color_mode, psapi.enum.ColorMode.rgb)

    def test_group_masks_round_trip(self):
        document = psapi.LayeredFile_16bit(psapi.enum.ColorMode.rgb, 16, 16)
        mask = np.arange(6 * 4, dtype=np.uint16).reshape(4, 6) * 1000
        group = psapi.GroupLayer_16bit("Masked", layer_mask=mask, width=6, height=4, pos_x=3, pos_y=2)
        document.add_layer(group)
        back = roundtrip(document)["Masked"]
        self.assertTrue(back.has_mask())
        np.testing.assert_array_equal(back.mask, mask)
        self.assertEqual((back.width, back.height), (6, 4))


class AdjustmentAndShapeApiTest(unittest.TestCase):
    def test_artboard_creation_settings_and_layer_blocks(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 64, 64)
        document.add_artboard(
            "Board",
            (0.0, 0.0, 40.0, 50.0),
            preset_name="Icon",
            background_type=1,
            guide_indices=[0, 2],
        )
        document.add_artboard(
            "Custom",
            (0.0, 40.0, 40.0, 64.0),
            background_type=4,
            background_color=(18.0, 108.0, 200.0),
        )
        self.assertEqual(len(document.artboards), 2)
        self.assertEqual(names(document.artboards), ["Custom", "Board"])
        document.move_layer("Custom")
        self.assertEqual(names(document.artboards), ["Board", "Custom"])
        board = document["Board"]
        self.assertEqual(board.kind, "artboard")
        self.assertTrue(board.is_artboard())
        self.assertEqual(
            board.artboard_info(),
            {
                "bounds": (0.0, 0.0, 40.0, 50.0),
                "preset_name": "Icon",
                "background_type": 1,
                "guide_indices": [0, 2],
                "background_color": None,
            },
        )
        custom = document["Custom"]
        self.assertEqual(custom.artboard_info()["background_color"], (18.0, 108.0, 200.0))
        block = board.artboard_block()
        self.assertEqual(block[0], "artb")
        board.set_artboard_block(*block)

        back = roundtrip(document)
        self.assertEqual(back["Board"].artboard_block(), block)
        self.assertEqual(back["Custom"].artboard_info()["background_type"], 4)
        settings = back.artboard_settings
        self.assertIsInstance(settings, bytes)
        self.assertTrue(back["Board"].clear_artboard())
        self.assertEqual([layer.name for layer in back.artboards], ["Custom"])
        self.assertNotEqual(back.artboard_settings, settings)
        back.clear_artboard_settings()
        self.assertIsNone(back.artboard_settings)

    def test_effect_block_setter_preserves_a_no_op_and_clears_mirrors(self):
        source = psapi.LayeredFile_8bit.read(
            FIXTURES / "documents" / "SmartObjects" / "smart_object_file_no_warp.psd"
        )
        layer = next(
            layer
            for layer in source.flat_layers
            if any(key in ("lfx2", "lmfx", "lfxs") for key, _ in layer.layer_effects_blocks())
        )
        blocks = layer.layer_effects_blocks()
        modern = next((key, payload) for key, payload in blocks if key in ("lfx2", "lmfx", "lfxs"))

        layer.set_layer_effects_block(*modern)
        self.assertEqual(layer.layer_effects_blocks(), blocks)
        self.assertEqual(
            roundtrip(source)[layer.name].layer_effects_blocks(),
            blocks,
        )
        layer.clear_layer_effects()
        self.assertEqual(layer.layer_effects_blocks(), [])

    def test_effect_payload_setter_keeps_unknown_fields_and_rejects_bad_payloads(self):
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 8, 8)
        document.add_layer(image("Effects"))
        layer = document["Effects"]

        def payload(value):
            # Version-16 descriptor with a future integer field and opaque tail.
            return (
                struct.pack(">III", 0, 16, 0)
                + struct.pack(">I4sI", 0, b"Lefx", 1)
                + struct.pack(">I4s4si", 4, b"Futr", b"long", value)
                + b"\xde\xad"
            )

        for value in (7, 8):
            expected = payload(value)
            layer.set_layer_effects_block("lmfx", expected)
            blocks = dict(layer.layer_effects_blocks())
            self.assertEqual(blocks["lmfx"], expected)
            self.assertIn("lrFX", blocks)
            self.assertEqual(dict(roundtrip(document)["Effects"].layer_effects_blocks()), blocks)

        before = layer.layer_effects_blocks()
        with self.assertRaises(ValueError):
            layer.set_layer_effects_block("lmfx", b"\x00" * 8)
        self.assertEqual(layer.layer_effects_blocks(), before)

    def test_adjustment_payloads_create_edit_and_round_trip(self):
        source = psapi.LayeredFile_8bit.read(
            FIXTURES / "generated" / "Adjustments" / "adjustment_layers_8bit.psd"
        )
        payload = source["Exposure"].adjustment_blocks()[0]
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 64, 64)
        document.add_adjustment_layer("Exposure", *payload)

        layer = document["Exposure"]
        self.assertEqual(layer.kind, "adjustment")
        self.assertTrue(layer.is_adjustment_layer())
        self.assertEqual(layer.adjustment_blocks(), [payload])
        layer.set_adjustment_block(*payload)
        self.assertEqual(roundtrip(document)["Exposure"].adjustment_blocks(), [payload])

        self.assertTrue(layer.clear_adjustment_block(payload[0]))
        self.assertFalse(layer.clear_adjustment_block(payload[0]))

    def test_legacy_and_modern_shapes_round_trip_from_typed_blocks(self):
        source = psapi.LayeredFile_8bit.read(
            FIXTURES / "generated" / "Vectors" / "vector_shapes_8bit.psd"
        )
        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 64, 64)
        expected = {}
        for source_name, name in [("Legacy Rectangle", "Legacy"), ("Ellipse", "Modern")]:
            source_layer = source[source_name]
            blocks = source_layer.adjustment_blocks() + source_layer.vector_blocks()
            document.add_shape_layer(name, (0, 0, 64, 64), blocks)
            expected[name] = (source_layer.adjustment_blocks(), source_layer.vector_blocks())

        reread = roundtrip(document)
        for name, (adjustment_blocks, vector_blocks) in expected.items():
            layer = reread[name]
            self.assertEqual(layer.kind, "shape")
            self.assertTrue(layer.is_shape_layer())
            self.assertEqual(layer.adjustment_blocks(), adjustment_blocks)
            self.assertEqual(layer.vector_blocks(), vector_blocks)


class TextRangeTest(unittest.TestCase):
    def test_ranges_split_runs_and_chain(self):
        layer = psapi.TextLayer_8bit("Title", "Hello World")
        result = layer.style_text("World").set_bold(True).set_font_size(40.0)
        self.assertIsInstance(result, psapi.CharacterStyleRange)
        self.assertEqual(layer.style_run_lengths(), [6, 5, 1])
        self.assertEqual(
            [layer.style_run_faux_bold(i) for i in range(layer.style_run_count)],
            [False, True, False],
        )
        self.assertEqual(layer.style_run(1).font_size, 40.0)
        self.assertEqual(layer.style_text("World").range_count(), 1)
        self.assertFalse(layer.style_text("absent").valid())
        layer.style_text("absent").set_bold(True)

        layer.style_range(0, 5).set_font("Courier")
        self.assertEqual(layer.style_run_lengths(), [5, 1, 5, 1])
        self.assertEqual(layer.font_postscript_name(layer.style_run_font(0)), "Courier")
        self.assertEqual(layer.font_postscript_name(layer.style_run_font(1)), "ArialMT")
        layer.paragraph_all().set_justification(psapi.enum.Justification.Center)
        self.assertEqual(layer.paragraph_run_justification(0), psapi.enum.Justification.Center)

        document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, 64, 64)
        document.add_layer(layer)
        back = roundtrip(document)["Title"]
        self.assertTrue(back.style_run_faux_bold(2))
        self.assertIsInstance(back.style_run(0), psapi.StyleRunProxy_8bit)

    def test_out_of_range_getters_are_none_and_setters_raise(self):
        layer = psapi.TextLayer_8bit("Title", "Hello")
        self.assertIsNone(layer.style_run_font_size(99))
        self.assertIsNone(layer.style_run_font_size(-1))
        with self.assertRaises(ValueError):
            layer.set_style_run_font_size(99, 12.0)
        with self.assertRaises(ValueError):
            layer.split_style_run(0, 99)


class OneBitDocumentTest(unittest.TestCase):
    """A 1-bit (bitmap mode) document reads as an 8-bit one.

    The minimal file below is a valid 1x1 grayscale bitmap document: the
    header declares depth 1 and the layer-and-mask section is empty. Its
    on-disk depth is reported through ``source_depth`` while ``bit_depth``
    stays 8, the depth it is read at.
    """

    @staticmethod
    def one_bit_bytes() -> bytes:
        import struct

        header = b"8BPS" + struct.pack(">H", 1) + bytes(6)
        header += struct.pack(">HIIHH", 1, 1, 1, 1, 1)  # channels, h, w, depth 1, grayscale
        return header + struct.pack(">I", 0) + struct.pack(">I", 0) + struct.pack(">I", 0)

    def test_reads_and_reports_the_source_depth(self):
        document = psapi.LayeredFile.read(io.BytesIO(self.one_bit_bytes()))
        self.assertIsInstance(document, psapi.LayeredFile_8bit)
        self.assertEqual(document.bit_depth, psapi.enum.BitDepth.bd_8)
        self.assertEqual(document.source_depth, psapi.enum.BitDepth.bd_1)

        # A file written back out is an ordinary 8-bit document.
        reread = psapi.LayeredFile_8bit.from_bytes(document.to_bytes())
        self.assertEqual(reread.source_depth, psapi.enum.BitDepth.bd_8)

        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "bitmap.psd")
            with open(path, "wb") as handle:
                handle.write(self.one_bit_bytes())
            self.assertEqual(
                psapi.PhotoshopFile.find_bitdepth(path), psapi.enum.BitDepth.bd_1
            )


class IoTest(unittest.TestCase):
    def test_streams_and_photoshop_file(self):
        document = psapi.LayeredFile_32bit(psapi.enum.ColorMode.rgb, 4, 4)
        document.add_layer(psapi.ImageLayer_32bit(np.ones((3, 4, 4), np.float32), "Layer", pos_x=2, pos_y=2))
        stream = io.BytesIO()
        document.write(stream)
        stream.seek(0)
        reread = psapi.LayeredFile.read(stream)
        self.assertIsInstance(reread, psapi.LayeredFile_32bit)
        np.testing.assert_array_equal(reread["Layer"][0], np.ones((4, 4), np.float32))

        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "file.psb")
            reread.write(path)
            self.assertEqual(psapi.PhotoshopFile.find_bitdepth(path), psapi.enum.BitDepth.bd_32)
            raw = psapi.PhotoshopFile()
            raw.read(psapi.util.File(path))
            copy = os.path.join(directory, "copy.psb")
            raw.write(psapi.util.File(copy))
            self.assertEqual(psapi.LayeredFile.read(copy)["Layer"].width, 4)
            with self.assertRaises(FileExistsError):
                reread.write(path, force_overwrite=False)
            with self.assertRaises(ValueError):
                reread.write(os.path.join(directory, "file.png"))

    def test_channel_id_info(self):
        info = psapi.util.ChannelIDInfo.from_id(psapi.enum.ChannelID.black, psapi.enum.ColorMode.cmyk)
        self.assertEqual(info.index, 3)
        info.set_index(-1, psapi.enum.ColorMode.cmyk)
        self.assertEqual(info.id, psapi.enum.ChannelID.alpha)
        with self.assertRaises(ValueError):
            psapi.util.ChannelIDInfo.from_id(psapi.enum.ChannelID.cyan, psapi.enum.ColorMode.rgb)

    def test_read_layers_expose_settings_from_fixtures(self):
        colors = psapi.LayeredFile.read(FIXTURES / "documents" / "LayerColor" / "layers_with_display_color.psd")
        self.assertTrue(all(layer.display_color == psapi.enum.LayerColor.violet for layer in colors.flat_layers))
        fill = psapi.LayeredFile.read(FIXTURES / "documents" / "BlendFill" / "blend_fill.psd")
        self.assertAlmostEqual(fill.layers[0].fill, 0.51, places=2)


class ReadMemoryLimitTest(unittest.TestCase):
    """The decoded-channel memory budget must be reachable from Python.

    Rust callers pass ``ReadOptions``; the bindings previously always used the
    default 2 GiB budget with no way to raise it or opt out, so a document over
    that size failed with no recourse.
    """

    def setUp(self):
        self.path = str(FIXTURES / "documents" / "BlendFill" / "blend_fill.psd")
        with open(self.path, "rb") as handle:
            self.data = handle.read()

    def test_from_bytes_accepts_an_explicit_byte_budget(self):
        # A budget too small for this document's channels is rejected.
        with self.assertRaises(ValueError):
            psapi.LayeredFile_8bit.from_bytes(self.data, memory_limit=1)

    def test_memory_limit_none_keeps_the_default_budget(self):
        # Omitting the argument must behave exactly as before the option existed.
        default = psapi.LayeredFile_8bit.from_bytes(self.data)
        explicit = psapi.LayeredFile_8bit.from_bytes(self.data, memory_limit=None)
        self.assertEqual(names(default.layers), names(explicit.layers))

    def test_memory_limit_zero_means_unlimited(self):
        unlimited = psapi.LayeredFile_8bit.from_bytes(self.data, memory_limit=0)
        self.assertEqual(names(unlimited.layers), names(roundtrip(psapi.LayeredFile_8bit.from_bytes(self.data)).layers))

    def test_generous_budget_reads_normally(self):
        generous = psapi.LayeredFile_8bit.from_bytes(self.data, memory_limit=64 * 1024 * 1024)
        self.assertTrue(generous.layers)

    def test_read_path_accepts_the_same_option(self):
        self.assertTrue(psapi.LayeredFile_8bit.read(self.path, memory_limit=0).layers)
        with self.assertRaises(ValueError):
            psapi.LayeredFile_8bit.read(self.path, memory_limit=1)

    def test_module_level_read_accepts_the_same_option(self):
        self.assertTrue(psapi.LayeredFile.read(self.path, memory_limit=0).layers)

    def test_negative_budget_is_rejected(self):
        with self.assertRaises(ValueError):
            psapi.LayeredFile_8bit.from_bytes(self.data, memory_limit=-1)

    def test_every_depth_exposes_the_option(self):
        # The macro is instantiated once per bit depth, so assert the keyword
        # exists structurally rather than inferring it from an error message.
        for document in (psapi.LayeredFile_8bit, psapi.LayeredFile_16bit, psapi.LayeredFile_32bit):
            self.assertIn("memory_limit", inspect.signature(document.from_bytes).parameters)
            self.assertIn("memory_limit", inspect.signature(document.read).parameters)
        self.assertIn("memory_limit", inspect.signature(psapi.read).parameters)


if __name__ == "__main__":
    unittest.main()
