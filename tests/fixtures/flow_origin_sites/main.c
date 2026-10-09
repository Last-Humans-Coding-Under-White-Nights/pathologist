#include "sites.h"

int g;

void moves(int **pp, struct node *n)
{
    int *a = &g;
    int *b = a;
    int *c = *pp;
    *pp = b;
    int *d = n->val;
    STORE(pp, c);
    (void)d;
    (void)next_of(n);
}

void tab1(void) {}
void tab2(void) {}
typedef void (*tab_fn)(void);

void table_init(void)
{
    tab_fn table[2] = {tab1, tab2};
    (void)table;
}

tab_fn ftable[2] = {tab1, tab2};
