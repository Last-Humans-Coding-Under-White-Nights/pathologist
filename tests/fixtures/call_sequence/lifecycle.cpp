struct Guard {
    Guard();
    ~Guard();
};

struct Item {
    Item();
    ~Item();
};

void work();
void after();
bool fail();
void use();
void recover();

void scoped()
{
    {
        Guard g;
        work();
    }
    after();
}

void early()
{
    Guard g;
    if (fail())
        return;
    work();
}

void loop()
{
    Item items[2];
    for (Item it : items) {
        use();
    }
}

void handler()
{
    try {
        work();
    } catch (Item e) {
        recover();
    }
}

struct Pair {
    Pair(int v);
    ~Pair();
};

int first();
int second();

void two()
{
    Pair a{first()}, b{second()};
}

struct BaseA {
    BaseA();
    ~BaseA();
};

struct BaseB {
    BaseB();
    ~BaseB();
};

struct Both : BaseA, BaseB {};

void bases()
{
    Both both;
    work();
}
