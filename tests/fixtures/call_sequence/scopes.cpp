struct Guard {
    Guard();
    ~Guard();
};

struct Node {
    Node(int v);
    ~Node();
};

bool more();
bool skip();
bool quit();
void work();
void after();
int value();

void jumps()
{
    {
        Guard g;
        if (quit())
            goto out;
        work();
    }
out:
    after();
}

void loops()
{
    while (more()) {
        Guard g;
        if (skip())
            continue;
        if (quit())
            break;
        work();
    }
    after();
}

void heap()
{
    Node *n = new Node(value());
    delete n;
}

struct Lease {
    Lease();
    ~Lease();
    explicit operator bool() const;
};

Lease lease();

void leased()
{
    while (Lease l = lease()) {
        work();
    }
    after();
}

void thrown()
{
    try {
        Guard g;
        if (quit())
            throw 1;
        work();
    } catch (...) {
        after();
    }
}

void retried()
{
    try {
    again:
        work();
    } catch (...) {
        if (more())
            goto again;
    }
}

void local_class()
{
    work();
    struct Local {
        int get() { return value(); }
    };
    after();
}

bool retry();
void before();

void rewound()
{
again:
    before();
    Guard g;
    if (retry())
        goto again;
    work();
}

void rewound_nested()
{
again:
    before();
    Guard outer;
    {
        Guard inner;
        if (retry())
            goto again;
        work();
    }
    after();
}

void forward()
{
    Guard g;
    if (quit())
        goto out;
    work();
out:
    after();
}
