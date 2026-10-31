namespace OHOS { namespace DataShare {
class IDataShare {
public:
    virtual int OpenFile(int uri) = 0;
};
class DataShareStub : public IRemoteStub<IDataShare> {
public:
    int OnRemoteRequest(int uri) { return OpenFileInner(uri); }
    int OpenFileInner(int uri) { return OpenFile(uri); }
};
}}
