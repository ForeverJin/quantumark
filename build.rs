// 构建：用 tonic-prost-build 把 CD2 的 proto 编成 gRPC 客户端。
//
// proto/clouddrive.proto 是 CD2 官方发布的接口定义（与 Full-Stack-Media-Library 里的
// 那份逐字节相同，版本标记 1.1.2）。本机没装 protoc，所以用 protoc-bin-vendored
// 自带的二进制，宿主机和镜像里都不依赖系统包。
//
// 只生成客户端，不生成服务端代码：这个程序只调用 CD2，不提供 gRPC 服务。

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/clouddrive.proto");

    let mut prost = prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);

    tonic_prost_build::configure()
        .build_server(false)
        .compile_with_config(
            prost,
            &[Path::new("proto/clouddrive.proto")],
            &[Path::new("proto")],
        )?;
    Ok(())
}
