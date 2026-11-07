//! The selector tables: for each command word a port can open, how many
//! argument words follow. Counts are those of the senders read.
//!
//! Ported from hw/misc/sgi-kona-ports.c in jamesbraid/qemu-sgi-origami.

use super::frame::Selector;

/// The kernel FIFO push port: the DMA packet, the list-write record and the pool
/// registration, then the display-mode and select-feed selectors.
pub(super) const DMA_FIFO: &[Selector] = &[
    Selector::new(0x1eb, 6), // dma_packet
    Selector::new(0x1c8, 2), // dma_list_write
    Selector::new(0x1ed, 3), // dma_pool
    Selector::new(0x1e6, 3),
    Selector::new(0x1f5, 0),
    Selector::new(0x1f6, 0),
];

/// The textport's commands, which follow a sync word.
pub(super) const TEXTPORT: &[Selector] = &[
    Selector::new(0x1, 1),
    Selector::new(0x2, 1),
    Selector::new(0x3, 4),
    Selector::new(0x4, 2),
    Selector::new(0x5, 4),
    Selector::new(0x6, 2),
    Selector::new(0x7, 2),
    Selector::new(0x8, 2),
    Selector::new(0x9, 1),
    Selector::new(0xc, 1),
];

/// The command page's tokens: kernel, graphics-process and X server commands.
pub(super) const KERNEL: &[Selector] = &[
    Selector::new(0x1f1, 3), // reason_page
    Selector::new(0x1f2, 3), // return_page
    Selector::new(0x1fd, 3), // context_map
    Selector::new(0x1f8, 1), // post_reason
    Selector::new(0x1dd, 1), // check_pipe
    Selector::new(0x1e9, 1), // get_config
    Selector::new(0x1ec, 1), // eeprom
    Selector::new(0x1e6, 3), // display_mode
    Selector::new(0x1fc, 1), // context_deactivate
    Selector::new(0x1f9, 4), // context_restore
    Selector::new(0x1f3, 11), // context_records
    Selector::new(0x1ee, 3), // context_words
    Selector::new(0x1f4, 2), // swap_pair
    Selector::new(0x1ed, 3), // dma_pool
    Selector::new(0x1eb, 6), // dma_packet
    Selector::new(0x1c8, 2), // dma_list_write
    Selector::new(0x1f5, 6), // select_feed_start
    Selector::new(0x1f6, 2), // select_feed_finish
    Selector::new(0x16d, 2), // list_call
    Selector::new(0x16e, 2), // list_call_pool
    Selector::new(0x170, 1), // list_calls
    Selector::new(0x171, 1), // list_calls_pool
    Selector::new(0x167, 4), // list_pull
    Selector::new(0x16f, 3), // list_move
    Selector::new(0x1e7, 1), // retrace
    Selector::new(0x1e4, 4), // mem_region
    Selector::new(0x1e1, 4), // mem_copy_down
    Selector::new(0x1e3, 8), // mem_swap_out
    Selector::new(0x1e2, 6), // mem_swap_in
    Selector::new(0x1df, 1), // mem_suspend
    Selector::new(0x1de, 0), // mem_resume
    Selector::new(0x172, 1), // resource_sync
    Selector::new(0x128, 3), // x_init
    Selector::new(0x129, 13), // x_gc
    Selector::new(0x12a, 4), // x_position
    Selector::new(0x145, 4), // x_bdata4
    Selector::new(0x12b, 5), // x_clip
    Selector::new(0x12d, 4), // x_scissor
    Selector::new(0x12e, 2), // x_color_map
    Selector::new(0x12f, 2), // x_gamma
    Selector::new(0x130, 1), // x_cursor
    Selector::new(0x131, 1),
    Selector::new(0x132, 2), // x_did_map
    Selector::new(0x133, 1), // x_did_config
    Selector::new(0x136, 7), // x_pixel_setup
    Selector::new(0x138, 2), // x_pixel_read
    Selector::new(0x139, 2), // x_pixel_write
    Selector::new(0x13a, 1), // x_pixel_finish
    Selector::new(0x137, 4), // x_pixel_copy
    Selector::new(0x13c, 1), // x_load_stipple
    Selector::new(0x13d, 3), // x_store_stipple
    Selector::new(0x13e, 3), // x_rotate_stipple
    Selector::new(0x14c, 1), // x_point
    Selector::new(0x14d, 6), // x_point6
    Selector::new(0x141, 1), // x_data1
    Selector::new(0x143, 3), // x_data3
    Selector::new(0x146, 5), // x_data5
    Selector::new(0x14e, 24), // x_point24
    Selector::new(0x15f, 1), // x_background_point
    Selector::new(0x160, 6), // x_background_point6
    Selector::new(0x161, 24), // x_background_point24
    Selector::new(0x14f, 1), // x_relative_point
    Selector::new(0x150, 6), // x_relative_point6
    Selector::new(0x151, 24), // x_relative_point24
    Selector::new(0x159, 2), // x_segment
    Selector::new(0x15a, 12), // x_segment6
    Selector::new(0x15b, 1),
    Selector::new(0x15c, 7),
    Selector::new(0x15d, 2),
    Selector::new(0x15e, 7),
    Selector::new(0x142, 2), // x_data2
    Selector::new(0x144, 4), // x_data4
    Selector::new(0x147, 6), // x_data6
    Selector::new(0x149, 8), // x_data8
    Selector::new(0x14b, 10), // x_data10
    Selector::new(0x154, 2), // x_box
    Selector::new(0x155, 12), // x_box6
    Selector::new(0x152, 2), // x_rect
    Selector::new(0x153, 12), // x_rect6
    Selector::new(0x156, 3), // x_spans
    Selector::new(0x157, 5), // x_spans2
    Selector::new(0x158, 13), // x_spans6
    Selector::new(0x1ff, 1), // wg_flush
    Selector::new(0x1a6, 1), // video
];

/// The GL client's commands, keyed by the low 16 bits of the opening word.
pub(super) const GL: &[Selector] = &[
    Selector::new(0x1, 10), // gl_buffer_config
    Selector::new(0x2, 1), // gl_pack
    Selector::new(0x4, 10), // gl_read_config
    Selector::new(0x114, 1), // gl_read_drawable
    Selector::new(0x177, 1), // gl_active_texture
    Selector::new(0x1d7, 4), // gl_tex_scale_bias
    Selector::new(0x17e, 1), // gl_tex_table
    Selector::new(0xee, 1), // gl_line_distance
    Selector::new(0x1ce, 1), // gl_lazy_mode
    Selector::new(0x19, 1), // gl_render_mode
    Selector::new(0x10c, 1), // gl_init_names
    Selector::new(0x10d, 1), // gl_load_name
    Selector::new(0xc2, 1), // gl_pop_name
    Selector::new(0xc3, 1), // gl_push_name
    Selector::new(0x17, 1), // gl_feedback_type
    Selector::new(0xc1, 1), // gl_pass_through
    Selector::new(0xe3, 4), // gl_viewport
    Selector::new(0xc, 4), // gl_scissor
    Selector::new(0xe4, 1), // gl_set_error
    Selector::new(0x1d, 4), // gl_clear_color
    Selector::new(0x1a, 1), // gl_clear
    Selector::new(0x1b, 4), // gl_clear_accum
    Selector::new(0xc5, 2), // gl_accum
    Selector::new(0x1c, 1), // gl_clear_index
    Selector::new(0x1e, 1), // gl_clear_stencil
    Selector::new(0x1f, 2), // gl_clear_depth
    Selector::new(0x57, 1), // gl_begin_first + 0
    Selector::new(0x58, 1), // gl_begin_first + 1
    Selector::new(0x59, 1), // gl_begin_first + 2
    Selector::new(0x5a, 1), // gl_begin_first + 3
    Selector::new(0x5b, 1), // gl_begin_first + 4
    Selector::new(0x5c, 1), // gl_begin_first + 5
    Selector::new(0x5d, 1), // gl_begin_first + 6
    Selector::new(0x5e, 1), // gl_begin_first + 7
    Selector::new(0x5f, 1), // gl_begin_first + 8
    Selector::new(0x60, 1), // gl_begin_last
    Selector::new(0x71, 1), // gl_end
    Selector::new(0x3, 1), // gl_rect
    Selector::new(0xc7, 1), // gl_flush
    Selector::new(0xc6, 1), // gl_finish
    Selector::new(0x1cc, 1), // gl_flush_raster
    Selector::new(0x3e, 1), // gl_get_error
    Selector::new(0x3b, 1), // gl_get_current
    Selector::new(0x3d, 1), // gl_get_raster
    Selector::new(0x1ca, 1), // gl_resolve_a
    Selector::new(0x1d6, 1), // gl_resolve_b
    Selector::new(0xcf, 4), // gl_depth_range
    Selector::new(0xd0, 1), // gl_matrix_mode
    Selector::new(0xd1, 12), // gl_frustum
    Selector::new(0xd2, 1), // gl_load_identity
    Selector::new(0xd3, 16), // gl_load_matrixf
    Selector::new(0xd4, 32), // gl_load_matrixd
    Selector::new(0xd5, 16), // gl_mult_matrixf
    Selector::new(0xd6, 32), // gl_mult_matrixd
    Selector::new(0xd7, 12), // gl_ortho
    Selector::new(0xd8, 1), // gl_pop_matrix
    Selector::new(0xd9, 1), // gl_push_matrix
    Selector::new(0xda, 8), // gl_rotated
    Selector::new(0xdb, 4), // gl_rotatef
    Selector::new(0xdc, 6), // gl_scaled
    Selector::new(0xdd, 3), // gl_scalef
    Selector::new(0xde, 6), // gl_translated
    Selector::new(0xdf, 3), // gl_translatef
    Selector::new(0x61, 1), // gl_color3b
    Selector::new(0x62, 6), // gl_color3d
    Selector::new(0x63, 3), // gl_color3
    Selector::new(0x64, 3), // gl_color3i
    Selector::new(0x66, 1), // gl_color3ub
    Selector::new(0x67, 3), // gl_color3ui
    Selector::new(0x6a, 8), // gl_color4d
    Selector::new(0x6b, 4), // gl_color4f
    Selector::new(0x6c, 4), // gl_color4i
    Selector::new(0x6d, 2), // gl_color4s
    Selector::new(0x6e, 1), // gl_color4ub
    Selector::new(0x6f, 4), // gl_color4ui
    Selector::new(0x72, 2), // gl_indexd
    Selector::new(0x73, 1), // gl_indexf
    Selector::new(0x74, 1), // gl_indexi
    Selector::new(0x75, 1), // gl_index_packed
    Selector::new(0x76, 1), // gl_normal3b
    Selector::new(0x77, 6), // gl_normal3d
    Selector::new(0x78, 3), // gl_normal3f
    Selector::new(0x79, 3), // gl_normal3i
    Selector::new(0x7a, 1), // gl_normal3s
    Selector::new(0x7c, 1), // gl_texcoord1f
    Selector::new(0x80, 2), // gl_texcoord2f
    Selector::new(0x82, 1), // gl_texcoord2s
    Selector::new(0x84, 3), // gl_texcoord3f
    Selector::new(0x88, 4), // gl_texcoord4f
    Selector::new(0x8b, 4), // gl_vertex2d
    Selector::new(0x8c, 2), // gl_vertex2
    Selector::new(0x8d, 2), // gl_vertex2i
    Selector::new(0x8e, 1), // gl_vertex2s
    Selector::new(0x8f, 6), // gl_vertex3d
    Selector::new(0x90, 3), // gl_vertex3f
    Selector::new(0x91, 3), // gl_vertex3i
    Selector::new(0x92, 1), // gl_vertex3s
    Selector::new(0x93, 8), // gl_vertex4d
    Selector::new(0x94, 4), // gl_vertex4f
    Selector::new(0x95, 4), // gl_vertex4i
    Selector::new(0x96, 2), // gl_vertex4s
    Selector::new(0xa7, 1), // gl_edge_flag
    Selector::new(0xa9, 2), // gl_raster_pos2f
    Selector::new(0xad, 3), // gl_raster_pos3f
    Selector::new(0xb1, 4), // gl_raster_pos4f
    Selector::new(0x10e, 15), // gl_raster_state
    Selector::new(0x28, 4), // gl_eval_attrib
    Selector::new(0x29, 8), // gl_eval_attrib2
    Selector::new(0x1d8, 1), // gl_list_pad1
    Selector::new(0x1d9, 3), // gl_list_pad3
    Selector::new(0x1da, 4), // gl_list_pad4
    Selector::new(0x1a7, 2), // gl_point_parameter
    Selector::new(0x1a8, 4), // gl_point_attenuation
    Selector::new(0x1a4, 2), // gl_sprite_mode
    Selector::new(0x1a5, 4), // gl_sprite_vector
    Selector::new(0x118, 2), // gl_sample_mask
    Selector::new(0x117, 1), // gl_sample_pattern
    Selector::new(0xef, 7), // gl_convolution_1d
    Selector::new(0xf0, 8), // gl_convolution_2d
    Selector::new(0xf2, 4), // gl_copy_convolution_1d
    Selector::new(0xf3, 5), // gl_copy_convolution_2d
    Selector::new(0xf4, 3), // gl_get_convolution
    Selector::new(0xf5, 9), // gl_separable
    Selector::new(0xf7, 3), // gl_get_separable
    Selector::new(0xf8, 3), // gl_convolution_parameter
    Selector::new(0xf9, 6), // gl_convolution_parameters
    Selector::new(0x116, 4), // gl_single_component
    Selector::new(0xe8, 9), // gl_colour_table
    Selector::new(0xea, 4), // gl_get_colour_table
    Selector::new(0xeb, 6), // gl_colour_table_parameters
    Selector::new(0xec, 2), // gl_get_colour_table_parameter
    Selector::new(0x102, 5), // gl_histogram
    Selector::new(0x103, 1), // gl_reset_histogram
    Selector::new(0x104, 1), // gl_get_histogram
    Selector::new(0x105, 2), // gl_get_histogram_parameter
    Selector::new(0x107, 2), // gl_minmax
    Selector::new(0x108, 1), // gl_reset_minmax
    Selector::new(0x109, 1), // gl_get_minmax
    Selector::new(0x10a, 1), // gl_get_minmax_parameter
    Selector::new(0x7, 6), // gl_lightf
    Selector::new(0x8, 6), // gl_lighti
    Selector::new(0x9, 5), // gl_light_modelf
    Selector::new(0xa, 5), // gl_light_modeli
    Selector::new(0x97, 6), // gl_material
    Selector::new(0xb5, 2), // gl_color_material
    Selector::new(0x24, 1), // gl_enable
    Selector::new(0x25, 1), // gl_disable
    Selector::new(0xc0, 1), // gl_shade_model
    Selector::new(0xce, 1), // gl_depth_func
    Selector::new(0x22, 1), // gl_depth_mask
    Selector::new(0xcb, 2), // gl_blend_func
    Selector::new(0xe6, 4), // gl_blend_color
    Selector::new(0xe7, 1), // gl_blend_equation
    Selector::new(0xca, 2), // gl_alpha_func
    Selector::new(0xcc, 3), // gl_stencil_func
    Selector::new(0xcd, 3), // gl_stencil_op
    Selector::new(0x20, 1), // gl_stencil_mask
    Selector::new(0x21, 4), // gl_color_mask
    Selector::new(0x23, 1), // gl_index_mask
    Selector::new(0x2e, 1), // gl_logic_op
    Selector::new(0xb6, 1), // gl_cull_face
    Selector::new(0xba, 1), // gl_front_face
    Selector::new(0xbf, 2), // gl_polygon_mode
    Selector::new(0x168, 2), // gl_polygon_offset
    Selector::new(0x11b, 2), // gl_offset_ext
    Selector::new(0xb, 3), // gl_polygon_stipple
    Selector::new(0xbd, 2), // gl_line_width
    Selector::new(0xbe, 2), // gl_point_size
    Selector::new(0xbc, 2), // gl_line_stipple
    Selector::new(0xb7, 5), // gl_fogf
    Selector::new(0xb8, 5), // gl_fogi
    Selector::new(0xb4, 9), // gl_clip_plane
    Selector::new(0xbb, 2), // gl_hint
    Selector::new(0xc4, 1), // gl_draw_buffer
    Selector::new(0x37, 1), // gl_read_buffer
    Selector::new(0x2f, 2), // gl_pixel_zoom
    Selector::new(0x30, 2), // gl_pixel_transfer
    Selector::new(0x33, 2), // gl_pixel_store
    Selector::new(0x34, 4), // gl_pixel_map_f
    Selector::new(0x35, 4), // gl_pixel_map_ui
    Selector::new(0x36, 4), // gl_pixel_map_us
    Selector::new(0x11, 5), // gl_tex_envf
    Selector::new(0x12, 5), // gl_tex_envi
    Selector::new(0x5, 8), // gl_bitmap_inline
    Selector::new(0x6, 9), // gl_bitmap
    Selector::new(0x3a, 8), // gl_draw_pixels
    Selector::new(0x39, 7), // gl_read_pixels
    Selector::new(0x38, 7), // gl_copy_pixels
    Selector::new(0x110, 4), // gl_copy_overlap
    Selector::new(0x52, 18), // gl_read_texels
    Selector::new(0x111, 2), // gl_mode_state
    Selector::new(0x115, 4), // gl_copy_convolution
    Selector::new(0x4c, 1), // gl_get
    Selector::new(0xc9, 1), // gl_push_attrib
    Selector::new(0xc8, 1), // gl_pop_attrib
    Selector::new(0x119, 1), // gl_pop_lights
    Selector::new(0x1c4, 1), // gl_tex_prefix
    Selector::new(0x1c5, 1), // gl_tex_cleanup_prefix
    Selector::new(0x174, 20), // gl_tex_load
    Selector::new(0x176, 1), // gl_tex_load_id
    Selector::new(0x178, 1), // gl_tex_bind_id
    Selector::new(0x179, 22), // gl_tex_bind_2d
    Selector::new(0x17b, 14), // gl_tex_bind_volume
    Selector::new(0x17c, 4), // gl_tex_bind_volume_short
    Selector::new(0x17d, 6), // gl_tex_detail
    Selector::new(0x1d5, 5), // gl_tex_bind
    Selector::new(0x106, 5), // gl_tex_bind_linear
    Selector::new(0xed, 4), // gl_tex_post_scale
    Selector::new(0x1e0, 2), // gl_tex_table_usage
    Selector::new(0x112, 1), // gl_igloo_irisgl_mode
    Selector::new(0x113, 1), // gl_igloo_lmc_color
    Selector::new(0x1a9, 1), // gl_igloo_swaptmesh
    Selector::new(0x1be, 1), // gl_igloo_colornormal
    Selector::new(0x1c9, 1), // gl_igloo_tmeshmode
    Selector::new(0x14, 9), // gl_tex_gen
    Selector::new(0xa8, 4), // gl_raster_pos2d
    Selector::new(0xaa, 2), // gl_raster_pos2i
    Selector::new(0xab, 2), // gl_raster_pos2s
    Selector::new(0xac, 6), // gl_raster_pos3d
    Selector::new(0xae, 3), // gl_raster_pos3i
    Selector::new(0xaf, 3), // gl_raster_pos3s
    Selector::new(0xb0, 8), // gl_raster_pos4d
    Selector::new(0xb2, 4), // gl_raster_pos4i
    Selector::new(0xb3, 4), // gl_raster_pos4s
    Selector::new(0x180, 1), // gl_array_state
    Selector::new(0x181, 1), // gl_array_begin
    Selector::new(0x182, 3), // gl_array_bulk
    Selector::new(0x1a3, 1), // gl_array_end
    Selector::new(0x184, 0),
    Selector::new(0x185, 0),
    Selector::new(0x186, 0),
    Selector::new(0x187, 0),
    Selector::new(0x188, 0),
    Selector::new(0x189, 0),
    Selector::new(0x18a, 0),
    Selector::new(0x18b, 0),
    Selector::new(0x18c, 0),
    Selector::new(0x18d, 0),
    Selector::new(0x18e, 0),
    Selector::new(0x18f, 0),
    Selector::new(0x190, 0),
    Selector::new(0x191, 0),
    Selector::new(0x192, 0),
    Selector::new(0x193, 0),
    Selector::new(0x194, 0),
    Selector::new(0x195, 0),
    Selector::new(0x196, 0),
    Selector::new(0x197, 0),
    Selector::new(0x198, 0),
    Selector::new(0x199, 0),
    Selector::new(0x19a, 0),
    Selector::new(0x19b, 0),
    Selector::new(0x19c, 0),
    Selector::new(0x19d, 0),
    Selector::new(0x19e, 0),
    Selector::new(0x19f, 0),
    Selector::new(0x1a0, 0),
    Selector::new(0x1a1, 0),
    Selector::new(0x1a2, 0),
];

/// The video command table: the data words each command carries between its
/// number and its checksum, or `None` where no sender was read and the checksum
/// must be searched for.
pub(super) const VIDEO: &[Option<u16>] = &[
    None, // 0x00 Test
    None, // 0x01 VCWrite
    None, // 0x02 VCRead
    None, // 0x03 DiagXmapColor
    None, // 0x04 DisableDiagXmapColor
    None, // 0x05 VCBusTrace
    None, // 0x06 DiagCursorFromImage
    None, // 0x07 DebugMask
    Some(3), // 0x08 SetupFrameBuffer
    Some(1), // 0x09 InputSync
    None, // 0x0a GetInputSync
    Some(2), // 0x0b HPhase
    Some(1), // 0x0c GetHPhase
    Some(1), // 0x0d VPhase
    Some(1), // 0x0e GetVPhase
    Some(2), // 0x0f ChannelGammaMap
    Some(1), // 0x10 GetChannelGammaMap
    None, // 0x11 Fetal
    Some(1), // 0x12 GetBoardConfig
    Some(1), // 0x13 GetChannelConfig
    Some(8), // 0x14 Resize
    Some(1), // 0x15 GetResize
    Some(383), // 0x16 LoadFormat
    Some(3), // 0x17 EnableChannel
    Some(1), // 0x18 DisableChannel
    Some(16), // 0x19 LoadGenlockParams
    Some(1), // 0x1a EnableGenlock
    None, // 0x1b CursorPosition
    None, // 0x1c CursorChannel
    None, // 0x1d CursorType
    Some(2), // 0x1e CursorPriority
    Some(2), // 0x1f ChannelBlanking
    Some(1), // 0x20 GetChannelBlanking
    Some(2), // 0x21 OutputGain
    Some(1), // 0x22 GetOutputGain
    Some(4), // 0x23 SyncSelects
    Some(1), // 0x24 GetSyncSelects
    Some(2), // 0x25 OutputPedestal
    Some(1), // 0x26 GetOutputPedestal
    None, // 0x27 InitTextport
    None, // 0x28 LoadTextportGlyph
    None, // 0x29 LoadTextportColorIndex
    None, // 0x2a EnableVocReadback
    Some(1), // 0x2b MasterClockSelect
    None, // 0x2c GetMasterClockSelect
    None, // 0x2d AutoInit
    Some(1), // 0x2e LoadEncoder
    None, // 0x2f EncoderOutputGain
    None, // 0x30 GetEncoderOutputGain
    Some(1), // 0x31 SCHPhase
    Some(1), // 0x32 GetSCHPhase
    Some(1), // 0x33 EncoderTestPattern
    Some(2), // 0x34 BindChannelToXWid
    None, // 0x35 LoadPabXilinx
    None, // 0x36 LoadPabSizeRegs
    None, // 0x37 LoadPabCtrlRegs
    Some(263), // 0x38 LoadDacCursor
    None, // 0x39 VampireAndFieldDominance
    Some(1), // 0x3a GetVampire
    Some(1), // 0x3b FastlistChunksize
    Some(1), // 0x3c TempBlankAllChannels
    Some(1), // 0x3d GetDGInterruptState
    Some(1), // 0x3e XMapClockSelect
    None, // 0x3f GetXMapClockSelect
    Some(3), // 0x40 ChannelResetRatios
    None, // 0x41 ResetRMBoards
    Some(1), // 0x42 GetChannelEnables
    Some(9), // 0x43 PlacementRes
    Some(1), // 0x44 GenlockSwitching
    Some(1), // 0x45 GetGenlockSwitching
    Some(2), // 0x46 ChannelFieldDominance
    None, // 0x47 GetChannelFieldDominance
    Some(1), // 0x48 BoardResetRatio
    None, // 0x49 GetBoardResetRatio
    Some(1), // 0x4a GetChannelFormatInfo
    Some(5), // 0x4b ChannelGammaMapState
    Some(1), // 0x4c GetChannelGammaMapState
    Some(1), // 0x4d EnableEncoder
    None, // 0x4e ResizeSyncType
    Some(7), // 0x4f Vampire
    Some(1), // 0x50 QueryMonitorName
    None, // 0x51 MonitorCommand
    Some(6), // 0x52 LoadXilinx
    Some(1), // 0x53 XServerVideoInit
    Some(1), // 0x54 InternalGenlock
    Some(1), // 0x55 GetInternalGenlock
    Some(3), // 0x56 BufferParams
    None, // 0x57 GetBufferParams
    Some(3), // 0x58 DataFormat
    None, // 0x59 GetDataFormat
    Some(1), // 0x5a DVPMult2Freq
    Some(42), // 0x5b VBOBCommand
    None, // 0x5c GetVBOBReply
];
