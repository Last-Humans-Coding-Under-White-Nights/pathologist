namespace OHOS { namespace DataShare {
class Interface {
public:
    virtual int OpenFile(int x) = 0;
};
class DataShareStub : public virtual Interface {};
class Backend : public virtual Interface {
public:
    int OpenFile(int x) override final { return x; }
};
class Provider : public DataShareStub, public Backend {};
class DataShareHelper {
public:
    int OpenFile(int x);
};
int use(DataShareHelper *h) { return h->OpenFile(1); }
}}
