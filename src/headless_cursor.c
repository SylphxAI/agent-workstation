/* Older headless outputs have no separate cursor plane. Skip the software
 * cursor draw calls, not the seat, cursor state or virtual pointer input.
 * The two signatures cover the legacy renderer and the scene render pass.
 * Do not link to wlroots or depend on its private structure layouts. */
struct wlr_output;
struct wlr_render_pass;
struct pixman_region32;

void wlr_output_render_software_cursors(struct wlr_output *output,
                                      struct pixman_region32 *damage) {
    (void)output;
    (void)damage;
}

void wlr_output_add_software_cursors_to_render_pass(struct wlr_output *output,
                                                   struct wlr_render_pass *pass,
                                                   const struct pixman_region32 *damage) {
    (void)output;
    (void)pass;
    (void)damage;
}
