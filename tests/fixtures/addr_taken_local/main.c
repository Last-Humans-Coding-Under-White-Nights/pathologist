/* Review of #127: forward dataflow passes through an address-taken local's
 * storage location (y -> x -> location of x -> ptr -> sink's p). */
void sink(int *p);

void test(void)
{
    int y = 5;
    int x = y;
    int *ptr = &x;
    sink(ptr);
}
