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
