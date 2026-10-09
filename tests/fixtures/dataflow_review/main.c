struct Ops { char *(*send)(char *); };
struct Connection { struct Ops *ops; };
char *identity(char *p) { return p; }
struct Ops operations = { .send = identity };
struct Connection connection = { .ops = &operations };
#define ID(v) identity(v)
void spelling(char *q, struct Connection *c) {
    char *r;
    r = q;
    struct { char *payload; } envelope;
    envelope.payload = r;
    r = identity(q); r = identity(r);
    char *(*fp)(char *) = identity;
    r = fp(q); r = fp(r);
    r = ID(q);
}
