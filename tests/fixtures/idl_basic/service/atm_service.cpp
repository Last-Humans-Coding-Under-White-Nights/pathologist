// The stub is generated from IAtm.idl; only the service is in the tree (#123).
#include "atm_stub.h"

namespace OHOS {
namespace Security {
class AtmService : public AtmStub {
public:
    int VerifyAccessToken(unsigned id, int &s) override;
    int LocalOnly();
};

int AtmService::VerifyAccessToken(unsigned id, int &s) { s = (int)id; return 0; }
int AtmService::LocalOnly() { return 1; }
} // namespace Security
} // namespace OHOS
