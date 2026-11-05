# Reviewed modern Nitro KMS/NSM helper source matrix. Revisions are peeled
# release-tag commits and hashes are Nix unpacked-source hashes.
{
  rust.version = "1.97.1";

  sdkC = {
    version = "0.4.6";
    tag = "v0.4.6";
    owner = "aws";
    repo = "aws-nitro-enclaves-sdk-c";
    rev = "f2dbc3fb1ab9b4496ff8ebc077d24e5f24265923";
    hash = "sha256-LMsopWrSEKgx62ak0lG6NVLo6gjD8MNP0RRFrL5zRfo=";
  };
  nsmApi = {
    version = "0.5.2";
    tag = "v0.5.2";
    owner = "aws";
    repo = "aws-nitro-enclaves-nsm-api";
    rev = "1993eeb0620d35f5cefc50b17638b432325328f9";
    hash = "sha256-aG8bWZ1LRPbPy5whMC0eLJkwZyObz3yKhDr7ZBpqNUY=";
  };
  awsLc = {
    version = "5.4.0";
    tag = "v5.4.0";
    owner = "aws";
    repo = "aws-lc";
    rev = "f6acf748df0ea6157d55e640730b38d21a7751cd";
    hash = "sha256-5++GRDTi2BQKEMyvPGm/+C3jKhyB0djUbUmI43107/c=";
  };
  s2nTls = {
    version = "1.7.7";
    tag = "v1.7.7";
    owner = "aws";
    repo = "s2n-tls";
    rev = "853d1943bbbd7f782a159e77830cdaaf520d68ed";
    hash = "sha256-e9qi7I8eg4CRVdjXIHnwDnl13RN9KQBor+UOiAEm8mg=";
  };
  awsCCommon = {
    version = "0.14.4";
    tag = "v0.14.4";
    owner = "awslabs";
    repo = "aws-c-common";
    rev = "3a5e638aee99c9f1d65696f7df8c4e5d3dc5805c";
    hash = "sha256-15nidQkcaWzkRSHuqWrD980cMSHcXP9snLjrgThNSdU=";
  };
  awsCSdkutils = {
    version = "0.2.9";
    tag = "v0.2.9";
    owner = "awslabs";
    repo = "aws-c-sdkutils";
    rev = "a1cc19f53b63658f1b1400b36f199eafeeb895a6";
    hash = "sha256-VxQB0KOtzkjV6n47E5x+/EeSZbWI+nZ4M2oJk0vWvL4=";
  };
  awsCCal = {
    version = "0.9.15";
    tag = "v0.9.15";
    owner = "awslabs";
    repo = "aws-c-cal";
    rev = "8aa2a48a09f93c65d4cf06388e143a6584de6321";
    hash = "sha256-n4FWrj3ssl64zrYHA1JJhFGmXH3uUziOfdEwjvruGeE=";
  };
  awsCIo = {
    version = "0.27.5";
    tag = "v0.27.5";
    owner = "awslabs";
    repo = "aws-c-io";
    rev = "e2946c99521fa12d285c9a0829c92b1bf713922b";
    hash = "sha256-baeoEZMiEdchgOgCeVjSAbh6XfOVwDNPanDFMXKemQY=";
  };
  awsCCompression = {
    version = "0.3.2";
    tag = "v0.3.2";
    owner = "awslabs";
    repo = "aws-c-compression";
    rev = "d8264e64f698341eb03039b96b4f44702a9b3f83";
    hash = "sha256-YckyQZNk+48g5jrT4q8Clmy4LRwswKONvFbVtJxgpYQ=";
  };
  awsCHttp = {
    version = "0.11.0";
    tag = "v0.11.0";
    owner = "awslabs";
    repo = "aws-c-http";
    rev = "8aefd899fc3210bfd0e3fd414011a3cb708bf6e4";
    hash = "sha256-SCdZfGIIHU6f0OArygZm0yY0wE6Hdx/JWvHZcK1DQOw=";
  };
  awsCAuth = {
    version = "0.10.4";
    tag = "v0.10.4";
    owner = "awslabs";
    repo = "aws-c-auth";
    rev = "4b5d524bf1a511b05e0fffe5bdc51800770b9427";
    hash = "sha256-qxZRGH+jHSrWAgKBMDdJQTn3bS23z94tgw/gO2IsSw4=";
  };
  jsonC = {
    version = "0.19";
    tag = "json-c-0.19-20260627";
    owner = "json-c";
    repo = "json-c";
    rev = "aa716cd8d663c976b99b0f30f102ee1d8ef63146";
    hash = "sha256-ZfwVOU6PJKHSj7XVZh5BUb3VJ+lHXZVMPdHh5fgrock=";
  };
}
