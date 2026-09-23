/* Issue #132: a static function's return flow must reach a destination
 * that is not a local of the calling function. */
struct XA { int v; };
struct XA global_a;
static struct XA *src_a = &global_a;

static void *ret_static(void) { return src_a; }

void *seen_a;                       /* global destination            */
static void *seen_static_a;         /* file-static destination       */
void *seen_init_a = ret_static();   /* file-scope initializer, no caller */

void use_a(void) {
    seen_a = ret_static();
    seen_static_a = ret_static();
}
