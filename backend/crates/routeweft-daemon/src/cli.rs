use crate::error::Error;
use crate::north::message::RouteRequest;

/// Historical parser help; the actual binary uses the dataset CLI.
pub const HELP: &str =
    "RouteWeft 历史编排解析接口。实际命令请运行 routeweft --help；查询必须显式提供数据集。";

#[derive(Debug)]
pub enum Command {
    Help,
    Route(RouteRequest),
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, Error> {
    let args: Vec<_> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Command::Help),
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        [command, profile, coordinates] if command == "route" => {
            let coordinates = coordinates
                .split(';')
                .map(|point| {
                    let (longitude, latitude) = point
                        .split_once(',')
                        .ok_or_else(|| Error::InvalidRequest("坐标格式应为 经度,纬度".into()))?;
                    let parse_number = |value: &str| {
                        value
                            .trim()
                            .parse::<f64>()
                            .map_err(|_| Error::InvalidRequest(format!("无法解析坐标：{value}")))
                    };
                    Ok((parse_number(longitude)?, parse_number(latitude)?))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            Ok(Command::Route(RouteRequest {
                coordinates,
                profile: profile.clone(),
            }))
        }
        _ => Err(Error::InvalidRequest(
            "命令格式错误，请使用 --help 查看帮助".into(),
        )),
    }
}
