#ifndef SITES_H
#define SITES_H
struct node {
    int *val;
    struct node *next;
};

static inline struct node *next_of(struct node *hn)
{
    struct node *hx = hn->next;
    return hx;
}

#define STORE(p, v) (*(p) = (v))
#endif
