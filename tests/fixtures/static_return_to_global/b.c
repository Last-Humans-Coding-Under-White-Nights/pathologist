/* A same-named static in another unit: use_b must see only b's definition
 * (AGENTS.md invariants 5 and 10). */
struct XB { int v; };
struct XB global_b;
static struct XB *src_b = &global_b;

static void *ret_static(void) { return src_b; }

void *seen_b;

void use_b(void) {
    seen_b = ret_static();
}
